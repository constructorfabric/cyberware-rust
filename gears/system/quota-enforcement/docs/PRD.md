<!-- Created: 2026-04-25 by Constructor Tech -->

# PRD — Quota Enforcement

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
  - [2.3 Actor Permissions](#23-actor-permissions)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Subject Model & Attribution](#31-subject-model--attribution)
  - [3.2 Metric Identity](#32-metric-identity)
  - [3.3 Integration Boundary with Usage Collector](#33-integration-boundary-with-usage-collector)
  - [3.4 Decision Contract for Calling Services](#34-decision-contract-for-calling-services)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Projection Contracts & Subject Attribution](#51-projection-contracts--subject-attribution)
  - [5.2 Quota Lifecycle](#52-quota-lifecycle)
  - [5.3 Quota Type Semantics](#53-quota-type-semantics)
  - [5.4 Time Period & Reset Semantics](#54-time-period--reset-semantics)
  - [5.5 Quota Operations: Debit, Credit, Rollback, Preview](#55-quota-operations-debit-credit-rollback-preview)
  - [5.6 Lease & Two-Phase Operations](#56-lease--two-phase-operations)
  - [5.7 Bulk Operations](#57-bulk-operations)
  - [5.8 Idempotency](#58-idempotency)
  - [5.9 Multi-Quota Evaluation, Quota Resolution Policy & Pluggable Engine](#59-multi-quota-evaluation-quota-resolution-policy--pluggable-engine)
  - [5.10 Quota Snapshot Read API](#510-quota-snapshot-read-api)
  - [5.11 Quota Enforcement Mode](#511-quota-enforcement-mode)
  - [5.12 Tenant Isolation](#512-tenant-isolation)
  - [5.13 Authorization](#513-authorization)
  - [5.14 Pluggable Storage Backend](#514-pluggable-storage-backend)
  - [5.15 Notification Plugin Contract](#515-notification-plugin-contract)
  - [5.16 Operational Telemetry](#516-operational-telemetry)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 Data Governance](#62-data-governance)
  - [6.3 NFR Exclusions](#63-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
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

The Quota Enforcement gear is the platform's authoritative engine for declaring resource consumption limits ("quotas")
and evaluating whether individual operations are permitted under those limits. It supports debit, credit, rollback, and
two-phase lease primitives and consumes declarative owner-scoped GTS projection contracts so quotas can be enforced
against tenants, users, and future organizational scopes without changing the evaluation engine.

Quota Enforcement is consumption-counter infrastructure only. It does not collect raw usage events, does not compute
pricing, does not own license-pack catalogs, plan templates, redistribution UIs, or increase-request workflows, and does
not decide enforcement responses on behalf of callers — it returns deterministic decisions (`Allowed` / `Denied`) or a
platform-canonical error that the calling service applies. Commercial overage and overage pricing (a tenant exceeds an
entitled allowance and is charged premium rates) are explicitly out of scope at the QE admission layer; that
responsibility belongs to the billing service downstream of QE, composed from Usage Collector observations and Quota
records. Integration with the Usage Collector (which is the source of consumption events) is performed by a separate
wrapper component, and quota management/provisioning concerns (catalog, plans, redistribution, increase-requests,
end-user and tenant-admin UIs) are owned by **Quota Manager** — both out of scope for this PRD; see §4.2.

### 1.2 Background / Problem Statement

Core CF/Gears (LLM Gateway, file-storage, compute, API gateway, etc.) increasingly need to enforce
resource budgets against a variety of subjects — tenants and individual users today, with richer organizational
structures (cost centers, applications, departments) anticipated as the platform's organizational model evolves. Typical
examples already in demand include: maximum AI tokens per day, maximum compute hours per month, maximum storage
allocation. Today each consuming service either implements ad-hoc counters in its own database (no consistency across
services, no operator visibility, no idempotency) or the budget is not enforced at all (costly overruns, no fairness
between tenants).

Without a centralized quota engine, every consuming team duplicates the same counter, ledger, period-rollover,
idempotency-key, and quota-resolution code — at varying levels of correctness. Cross-service quota policies (a single "
AI tokens per tenant" budget that spans multiple AI-using services) are impossible to express because each service holds
its own counter. Operator workflows for raising or lowering caps require a per-service migration. The Quota Enforcement
gear addresses these problems by providing a single, shared, multi-tenant counter/ledger backend with explicit Quotas,
idempotent operations, and a uniform evaluation contract.

### 1.3 Goals (Business Outcomes)

- **Single source of truth for resource budgets**: All consuming platform services evaluate budgets against Quota
  Enforcement rather than maintaining their own counters; one operator change to a quota cap takes effect across every
  service that consults it.
- **Idempotent, race-safe accounting**: Every debit, credit, rollback, and lease operation is replay-safe under
  at-least-once delivery semantics, so retried network calls or duplicate background-job invocations do not corrupt
  counters.
- **Operator self-service for new quota types**: Platform operators can define new metric quotas at runtime and assign
  them to tenants or users without code changes, redeployment, or per-service rollout.
- **Predictable enforcement decisions for consumers**: Consumers receive deterministic decisions (`Allowed` / `Denied`)
  within a bounded latency budget, with explicit reason codes, so they can integrate quota checks into hot paths without
  bespoke fallback logic. Operational failures (Engine timeout, mid-flight infrastructure unavailability, invariant
  violations) surface as platform-canonical errors with the same fail-closed discipline.

**Success Metrics** (measured at initial production release):

| Goal                              | Measurable Success Criterion                                                                                                                                                         | Target                                |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------- |
| Single source of truth            | All in-scope platform services that previously held local quota counters integrated with Quota Enforcement; zero per-service quota counters remaining at first production deployment | 100% migration of in-scope services   |
| Idempotent accounting             | Zero double-counting incidents observed in soak testing under simulated retry storms (10× normal RPS with 5% retry rate) over a 7-day period                                         | 0 double-count events over 7-day soak |
| Operator self-service             | Time from API call defining a new quota to first evaluable operation against it                                                                                                      | ≤ 1 minute end-to-end                 |
| Predictable enforcement decisions | p95 latency of `debit` operation under target load                                                                                                                                   | ≤ 100ms p95 at 10 000 ops/sec         |

### 1.4 Glossary

| Term                      | Definition                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
|---------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Subject                   | A concrete identity to which a Quota is assigned, identified by `(projection_type, subject_id)`. The consumer supplies a scope-kind and opaque id; QE derives the projection type from the metric-owner catalogue after PDP authorization.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| Subject Projection        | A registered, versioned GTS type derived from `gts.cf.core.qe.subj.v1~`. It declares one scope and its admitted metrics. Callers never select it directly; operation metadata is defined once by the metric request contract.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Metric Request Contract   | The single owner-published GTS contract for one metric, derived from `gts.cf.core.qe.request.v1~`. It validates operation-level metadata and attaches the metric's arbitration constraint contract.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| Resource Projection       | A registered, versioned GTS type derived from `gts.cf.core.qe.res.v1~` that describes the optional resource identity and request properties available to Policies. It does not enter the P1 counter key.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| Projection                | A metric-owner-declared GTS contract that maps a logical scope to the concrete type used in Quota identity. Consumers send logical attribution and operation metadata; QE performs the catalogue mapping.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| Subject Attribution       | The caller-supplied `tenant_id` plus additional `{kind, id}` subject refs. The PDP authorizes this tuple against the authenticated service principal before QE maps it to projections.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| Metric                    | The named resource being counted. Metric names are registered as usage types in the platform `types-registry`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| Quota                     | A declarative limit assigned to a single subject for a single metric. Carries the subject reference, metric reference, quota type, period (consumption types only), enforcement mode, cap, optional notification thresholds, optional validity window, and optional failure-mode hint. Identified by a stable quota ID. Quotas are first-class stored entities — there is no separate "template" or "binding" concept; an operator who wants the same shape on many subjects creates one Quota per subject (typically driven by the subject manager at subject-creation time).                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Quota Snapshot            | A point-in-time per-Quota view returned by the snapshot read APIs (§5.10): for each applicable Quota — `quota_id`, `cap` (numeric or `null` for unbounded Quotas), current consumed, `remaining` (numeric or `null`), period boundary, validity window, metadata. Engine-agnostic — no aggregate "headline" cap/balance is computed, since under cascade, split, or attribute-gated Engines no single number is universally meaningful. Authoritative admission for a specific operation is given by the Engine's `Decision` (or its read-only `Preview`, `cpt-cf-quota-enforcement-fr-evaluate-preview`).                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Quota Resolution Policy   | An operator-managed entity that binds a Quota Resolution Engine and its config to a scope. P1 supports two scope levels: platform default (`global`) and per-metric. The most-specific-scope Policy applies at evaluation time. The scope ladder is intentionally extensible — narrower scopes (e.g., per-subject) are deferred and tracked in §13 Open Questions. See `cpt-cf-quota-enforcement-fr-quota-resolution-policy` for versioning, scope precedence, and seeded defaults.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Quota Resolution Engine   | The pluggable component that implements multi-Quota arbitration logic. Receives the applicable-Quotas set, current usage, the request, and an opaque engine-specific config; returns a Decision (result + debit plan + diagnostics). P1 ships two built-ins: `most-restrictive-wins` (hardcoded; fastest path; produces a single-entry Debit Plan against the binding Quota — see §5.9) and `cel` (sandboxed CEL evaluator; customizable). Multi-Quota debit patterns (cascade, attribute-weighted splits) are expressed via a `cel` Policy or future Engines. Additional engines (Starlark, Lua, Wasm-loaded) plug in via the same trait without changes to evaluation core.                                                                                                                                                                                                                                                                                                                                                                                                         |
| Debit Plan                | A `{quota_id → QuotaDebitPlan}` map produced by the active Engine alongside the result verdict. `QuotaDebitPlan` carries `amount` (total counter mutation for that Quota; ≥ 0, integer). The struct is extension-ready — additional per-Quota fields (e.g., a `clamped` marker if cap-clamp admission lands in P3) MAY be added in future phases without breaking the top-level Decision shape. The system mutates counters strictly per the Debit Plan; Quotas not named in the plan are not touched. P1 invariants enforce per-entry `0 ≤ amount ≤ request.amount` (integer arithmetic; see `cpt-cf-quota-enforcement-fr-quota-resolution-engine`). The per-entry cap prevents accidental over-charge of any single counter; the system does **not** constrain `sum(amount)`, leaving operators free to express either sum-semantics (one operation distributed across pools — cascade, proportional split: `sum(amount) = request.amount`) or multi-counter / AND-semantics (each applicable pool tracks the same operation independently: `sum(amount) = N × request.amount`) through the active Engine. |
| Quota Cascade / Spillover | An arbitration pattern where one Quota acts as primary pool and another as fallback; debit is routed to the primary first and falls through to the fallback only when the primary's remaining capacity is insufficient. P1 ships two cascade capabilities: the default `most-restrictive-wins` Engine implements subject-scope cascade (more-specific subject scope wins; P1: user-scope > tenant-scope) with single-entry Debit Plans; the customizable `cel` Engine produces arbitrary multi-entry plans for cross-tier splits (primary takes X, fallback takes `amount − X`), intra-tier cascade (between same-scope Quotas identified by metadata), and proportional distributions.                                                                                                                                                                                                                                                                                                                                                                                               |
| Arbitration Constraints   | Operator-supplied constraints attached to a Quota at create/update, validated once against the metric owner's constraint GTS contract and then snapshotted from storage for Engine evaluation. Their business meaning remains opaque to QE core.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Request Metadata          | One required caller-supplied operation-level object, validated for subject-based evaluation requests against the metric owner's request contract. It is distinct from arbitration constraints because the two sides may intentionally use different shapes and cardinalities.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| Allocation Quota          | A quota type that bounds in-flight reservable capacity (e.g., maximum concurrent jobs). No periodic reset.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| Consumption Quota         | A quota type that bounds cumulative consumption within a recurring time period. Counter resets at the period boundary.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| Rate Quota                | A quota type that bounds the rate of operations over a sliding or windowed interval. P3 type; not implemented in P1.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| Enforcement Mode          | The Quota's behavior at the cap boundary.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| Period                    | The recurring time window for consumption-type Quotas. UTC, calendar-aligned by default.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| Debit                     | An idempotent operation that decreases a Quota's remaining capacity (allocation: increment in-flight; consumption: increase used-amount in current period).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| Credit                    | An idempotent operation that increases a Quota's remaining capacity (allocation: decrement in-flight; consumption: decrease used-amount).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Rollback                  | An idempotent operation that reverses a previously committed debit or lease, identified by the original idempotency key. Equivalent to refund-on-cancel.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| Lease                     | A two-phase operation that holds capacity for a bounded TTL without committing. Resolved by `commit` (converts the lease to a debit), `release` (returns capacity to the Quota), or auto-release on TTL expiry.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| CEL                       | Common Expression Language. Used to express custom Quota Resolution Policies.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| Idempotency Key           | A client-supplied identifier ensuring that duplicate submissions of the same operation produce a single counter effect. Required on every debit, credit, rollback, reserve, commit, and release call.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| Notification Plugin       | An in-process Rust trait implemented by deployment-specific event sinks. P1 emission mechanism; P2 will additionally route through the platform EventBus.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Fail-Closed               | Failure mode in which authorization or evaluation errors result in operation denial. Quota Enforcement itself is fail-closed on internal errors; the consuming service decides its own behavior on Quota Enforcement unavailability.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| PDP                       | Policy Decision Point — the platform authorization service (`authz-resolver`) that evaluates access control policies and returns permit/deny decisions, optionally with constraint filters that narrow the scope of permitted operations.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| SecurityContext           | A platform-provided server-side structure carrying the authenticated caller principal: a service principal on the consumer S2S surface and a user/operator principal on management surfaces. Derived from authentication, never from request payloads; explicit target attribution is a separate PDP-authorized input.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |

## 2. Actors

### 2.1 Human Actors

Quota Enforcement is a server-side engine. Only one human actor — the Platform Operator — interacts with QE directly.
Tenant administrators and end users interact with the platform via **Quota Manager** (a separate platform component, out
of scope for this PRD); Quota Manager translates their workflows into QE API calls.

#### Platform Operator

**ID**: `cpt-cf-quota-enforcement-actor-platform-operator`

- **Role**: Defines quota types, configures storage backend, sets default quota-resolution
  policy, registers notification sinks, and monitors overall Quota Enforcement health.
- **Needs**: APIs for declarative quota lifecycle management; visibility into per-quota utilization and
  rejection rates; ability to onboard new metric quotas without redeployment.

> Tenant Administrator and End User are recognized human roles at the platform level but are **not direct actors** of
> Quota Enforcement. Their workflows (assigning caps within a tenant, reading personal balances, requesting cap
> increases, redistributing tenant top-ups) are mediated by Quota Manager
> (`cpt-cf-quota-enforcement-actor-quota-manager`). When this PRD describes capabilities such as "assign quotas to users
> in a tenant" or "end user reads personal balance", those capabilities are exposed by Quota Manager and implemented
> internally by Quota Manager calling QE's APIs as a Quota Manager actor.

### 2.2 System Actors

#### Quota Consumer

**ID**: `cpt-cf-quota-enforcement-actor-quota-consumer`

- **Role**: Any authenticated platform service that performs operations subject to budget enforcement (e.g., LLM
  Gateway invoking a model, compute service starting a job, storage service allocating bytes). It calls QE directly
  with an explicit target tenant, subject refs, and operation metadata. The PDP authorizes that attribution tuple
  against the service principal. End users access quota views through the consuming service, never QE directly.

#### Metric Owner

**ID**: `cpt-cf-quota-enforcement-actor-metric-owner`

- **Role**: The single Gear that owns a metric. Publishes and registers the owner-scoped subject/resource projections,
  one metric request contract, and its attached constraint contract, including admitted metrics and supported scopes. Its schemas must be
  general enough for every service that debits the shared metric. It publishes the contract but does not proxy calls to
  QE; any authorized caller may debit directly.

#### Quota Reader

**ID**: `cpt-cf-quota-enforcement-actor-quota-reader`

- **Role**: Any system that queries Quota Snapshot data without modifying counters (e.g., dashboards, billing systems,
  throttling proxies). Consumes the read API only.

#### Quota Manager

**ID**: `cpt-cf-quota-enforcement-actor-quota-manager`

- **Role**: Platform component that owns license-pack catalog, plan templates, provisioning, redistribution,
  increase-request workflow, and the human-facing surfaces for tenant administrators and end users. Calls Quota
  Enforcement's management CRUD/read APIs on behalf of tenant administrators under user-plane PDP scope. For an
  end-user view, its backend calls the consumer S2S snapshot API with its service principal and explicit target.
  It resolves the owning Gear for each metric so Quotas are created against that owner's projection, and it orchestrates
  replacement of Quotas when a future release supports breaking projection activation. That workflow is out of P1 and
  is described here only because QE's actor model and permission table refer to it.
- **Needs**: Stable Quota CRUD APIs (create, update, deactivate, read), bulk Quota Snapshot reads with PDP scoping,
  Quota Snapshot reads scoped to a single user/tenant, credit primitive for redistribution and compensatory adjustments.

#### Subject Manager

**ID**: `cpt-cf-quota-enforcement-actor-subject-manager`

- **Role**: Platform service that owns the lifecycle of subject identities (e.g., `account-management` for
  tenants and users). Subject managers signal subject lifecycle events (created, removed, identity changes) to **Quota
  Manager**, which decides whether to materialize plan templates into concrete Quotas, deactivate Quotas on subject
  removal, or perform any other reaction; Quota Manager translates those decisions into Quota Enforcement CRUD calls.
  **In P1 there is no direct Subject Manager → Quota Enforcement channel** — the subject-manager-issued lifecycle stream
  reaches QE solely via Quota Manager-mediated CRUD. Neither subject managers nor Quota Enforcement owns the
  template/catalog concept (see §4.2 umbrella delegation).

#### Storage Backend

**ID**: `cpt-cf-quota-enforcement-actor-storage-backend`

- **Role**: The persistent store backing Quota Enforcement's Quotas, counters, leases, Quota Resolution Policies, and
  operation log. P1: `toolkit-db`-based plugin; alternative backends are viable under the same plugin contract
  (`cpt-cf-quota-enforcement-contract-storage-plugin`).

#### Notification Sink

**ID**: `cpt-cf-quota-enforcement-actor-notification-sink`

- **Role**: A deployment-specific implementation of the Quota Enforcement notification plugin contract. Receives events
  on threshold crossings, period rollovers, lease auto-release, lease-resolved-by-deactivation, and definition changes.
  P1: in-process plugin trait. P2: standardized routing through the platform EventBus.

#### Types Registry

**ID**: `cpt-cf-quota-enforcement-actor-types-registry`

- **Role**: The platform `types-registry` gear. Provides metric (usage type) registration, kind classification (
  counter/gauge), and enforcement-mode classification (`QuotaGated` / `Direct`); Quota Enforcement references registered
  metric names in Quotas. It also hosts the QE abstract bases, scope discriminators, owner projections, their
  admitted-metric traits, metric request contracts, and attached constraint contracts. QE does not duplicate these schemas in a table or
  registration API.

#### AuthZ Resolver

**ID**: `cpt-cf-quota-enforcement-actor-authz-resolver`

- **Role**: Platform PDP that authorizes every Quota Enforcement read and write operation, optionally returning
  constraint filters that scope read results.

#### Monitoring System

**ID**: `cpt-cf-quota-enforcement-actor-monitoring-system`

- **Role**: Consumes Quota Enforcement telemetry (metrics, health endpoints, structured logs) for dashboards, alerting,
  and operational visibility.

### 2.3 Actor Permissions

| Actor                                              | Permitted Operations                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | Denied by Default                                                                                                                                                                                                                         |
| -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cpt-cf-quota-enforcement-actor-platform-operator` | Create and update Quotas; manage Quota Resolution Policies; view utilization across authorized tenants; register notification sinks                                                                                                                                    | Bypassing tenant scope without explicit PDP grant; deleting the seeded global Policy; registering owner projections through QE                                            |
| `cpt-cf-quota-enforcement-actor-metric-owner`      | Publish projections, per-metric request contracts, and attached constraint contracts through `types-registry`; version them compatibly                                                                                                                              | Creating caller-specific projections for its shared metric; using projection traits as a substitute for PDP permission                                                     |
| `cpt-cf-quota-enforcement-actor-quota-manager`     | Acting on behalf of tenant administrators: create, update, and deactivate Quotas within the tenant's scope (subject to operator-imposed bounds and PDP scope); invoke credit and preview; read authorized snapshots; resolve the owning projection for each metric. | Modifying quotas of other tenants; publishing owner contracts; managing Quota Resolution Policies; bypassing PDP scope of the original caller |
| `cpt-cf-quota-enforcement-actor-quota-consumer`    | Invoke debit, credit, rollback, reserve, commit, release, batch debit, consumer `evaluate_preview`, and consumer snapshot reads for a caller-supplied attribution tuple authorized by PDP                                                                                                                                                                                                                                                                                                                 | Invoking management-only operations; operating outside the PDP-authorized tenant, subject, metric, or resource scope                                                                                                                       |
| `cpt-cf-quota-enforcement-actor-quota-reader`      | Perform manager/operator snapshot reads and previews with an explicit target, within PDP-returned scope                                                                                                                                                                                                                                                                                                                                                                                                 | Modifying counters; reading raw operation logs unless explicitly authorized                                                                                                                                                               |
| `cpt-cf-quota-enforcement-actor-subject-manager`   | Signal subject lifecycle events (created, removed, identity changes) to Quota Manager, which translates them into Quota Enforcement CRUD calls. P1: no direct Subject Manager → Quota Enforcement channel — see `cpt-cf-quota-enforcement-contract-subject-manager` (informational) and §4.2 umbrella delegation                                                                                                                                                                                          | Calling Quota Enforcement directly in P1; mutating quota counters or definitions; reading Quota Enforcement state outside Quota Manager-mediated read APIs                                                                                |
| `cpt-cf-quota-enforcement-actor-storage-backend`   | Receive and persist Quotas, counters, Quota Resolution Policies, leases, idempotency records, and operation logs forwarded by the Quota Enforcement gateway plugin                                                                                                                                                                                                                                                                                                                                        | Direct access from any actor other than the authorized storage plugin instance                                                                                                                                                            |
| `cpt-cf-quota-enforcement-actor-notification-sink` | Receive structured events emitted by Quota Enforcement on threshold crossings, period rollovers, lease auto-release, lease resolution by quota deactivation, and definition changes                                                                                                                                                                                                                                                                                                                       | Initiating operations against Quota Enforcement; reading quota state directly                                                                                                                                                             |
| `cpt-cf-quota-enforcement-actor-types-registry`    | Respond to metric registration and validation requests initiated by the Quota Enforcement gateway                                                                                                                                                                                                                                                                                                                                                                                                         | N/A — passive service; does not initiate operations on Quota Enforcement                                                                                                                                                                  |
| `cpt-cf-quota-enforcement-actor-authz-resolver`    | Respond to PDP authorization queries initiated by the Quota Enforcement gateway                                                                                                                                                                                                                                                                                                                                                                                                                           | N/A — passive PDP service                                                                                                                                                                                                                 |
| `cpt-cf-quota-enforcement-actor-monitoring-system` | Read observability endpoints (`/health/*`, metrics scrape)                                                                                                                                                                                                                                                                                                                                                                                                                                                | Modifying counters or definitions                                                                                                                                                                                                         |

Authorization is enforced via the platform PDP (`authz-resolver`) on all read and write operations. Unauthenticated
requests are rejected before any authorization check. Authenticated requests have their public request/target shape
checked before PDP; catalogue and contract validation follows authorization. Failures result in immediate rejection with
no partial operation (fail-closed; see §3.4 — "Quota Enforcement itself fails closed on internal errors").

## 3. Operational Concept & Environment

### 3.1 Subject Model & Attribution

Every Quota is bound to a concrete subject identified by `(projection_type, subject_id)`. The projection is a concrete
GTS type derived from QE's abstract `gts.cf.core.qe.subj.v1~` base and declared by the Gear that owns the metric.
Consumer requests supply `tenant_id` and additional `{kind,id}` subjects; QE maps `(metric, kind)` to the concrete
projection only after public request-shape checks pass and PDP authorizes the complete tuple for the authenticated
service principal. A resource projection may also be supplied, but resource is not part of the P1 counter key.

An owner publishes separate scope projections, for example
`gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~` and
`gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.tenant.v1~`. Every caller of the owner's metric uses these same contracts.
An operation by user `U` in tenant `T` can therefore resolve both `(owner-user-projection, U)` and
`(owner-tenant-projection, T)` and present every matching Quota to the Engine. This preserves user→tenant cascade and,
because the projection does not vary by caller, preserves a shared cross-service counter.

The concrete projection type does not arrive on the wire. `kind` is a well-known QE scope instance; the catalogue's
unique `(metric, scope)` index selects the projection. The tenant scope is materialized from `tenant_id`, while
`subjects` carries additional applicable scopes. Missing, duplicate, unknown, or unadmitted kinds fail before
evaluation. A future derivation or attestation service may wrap this raw API without changing it.

This design has the following consequences that callers and operators must understand:

1. **Tenant-scoped owner projections apply to every user in the tenant.** A Quota whose subject is
   `(owner-tenant-projection, T)` constrains every
   operation performed within tenant `T`, regardless of which user performs it. Each operation's Decision arbitrates
   tenant-level and user-level Quotas via the active Quota Resolution Policy.
2. **User-scoped owner projections constrain a single user within their tenant.** A Quota whose subject is
   `(owner-user-projection, U)` constrains
   only authorized operations attributed to user `U`, and never crosses the tenant boundary that owns it.
   Under the default `most-restrictive-wins` Engine, user-scope has higher priority than tenant-scope in binding-Quota
   selection (see §5.9).
3. **Attribution is explicit and PDP-authorized.** The caller supplies tenant and subject ids, but they remain
   untrusted until PDP authorizes the complete tuple against the authenticated service principal. Callers never select
   concrete projections.
4. **Arbitration logic lives in the Engine, not in Quota records.** The relationship between Quotas of different
   projection scopes — independence, cascade, proportional split, or any other rule — is determined by the active Quota
   Resolution Engine and its config, not by attributes on the Quota records themselves. Quota records remain pure
   declarative caps; multi-Quota arbitration is a separate concern owned by the Policy/Engine layer.

#### Phase Roadmap for Projection Scopes

| Phase | Projection scopes | Resolution mechanism |
|-------|-------------------|----------------------|
| P1 | Metric-owner-declared `tenant` and `user` projections as required by each owner | Consumer supplies `tenant_id` and additional `{kind, id}` refs; QE maps `(metric, kind)` through its validated catalogue after PDP authorization |
| P2 | Additional owner-scoped flat projections such as `service-account`, `client-app`, or `api-key` | Add a scope instance and registered owner projection; the raw consumer envelope remains unchanged |
| P3 | Owner projections backed by resource-group hierarchy, such as `cost-center` or `department` | A separate wrapper may derive or attest the same raw subject refs without changing QE core |

New scopes extend projection resolution, not the subject identity model. The evaluation engine continues to receive an
applicable-Quota set and does not implement registry traversal.

### 3.2 Metric Identity

Metric names in Quota Enforcement are not internally minted. Every metric referenced by a Quota is the registered name
of a usage type in the platform `types-registry` gear. Metric instances follow the GTS URI form under base
`gts.cf.qe.metric.type.v1~` — e.g., `gts.cf.qe.metric.type.v1~cf.qe.metric.ai_tokens_input.v1`,
`gts.cf.qe.metric.type.v1~cf.qe.metric.vcpu_hours.v1`, `gts.cf.qe.metric.type.v1~cf.qe.metric.storage_bytes.v1`.

> **Notation.** Throughout this document, metric instances are referenced by their short instance names
> (`ai-tokens-input`, `vCPU-hours`, `storage-bytes`, etc.) in examples, and use-case payloads for readability.
> API requests, storage rows, and outbox events use the full GTS URI form shown above.

Quota Enforcement itself treats the metric name as an opaque string at evaluation time. At Quota creation time, Quota
Enforcement optionally validates that the referenced metric name exists in `types-registry`; an unknown metric is
reported as an actionable creation-time error. The format of the metric name (length, allowed characters, namespace
conventions) is governed entirely by `types-registry` — Quota Enforcement inherits whatever format the registry permits
and adds no additional naming rules of its own. The `cf.qe.metric.*` namespace used in this document is provisional
pending platform-wide alignment (§13 Open Questions).

Every metric has one owning Gear. Its subject projections declare the metrics they admit, allowing `types-registry` to
enumerate the owner, supported scopes, and request schemas. This does not settle metric identity: owner-namespacing is
only a candidate for the open cross-gear namespace decision, and the registry remains authoritative.

The metric kind reported by `types-registry` (`counter` vs `gauge`) is informative for operators choosing the
appropriate quota type:

- counter-kind metrics naturally pair with **consumption** Quotas (cumulative within period),
- gauge-kind metrics naturally pair with **allocation** Quotas (in-flight reservable capacity).

Quota Enforcement does not enforce a hard pairing — operators may declare any combination — but the read API includes
the metric kind in Quota responses to surface mismatches.

#### Gated vs Non-Gated Metrics

The `types-registry` additionally classifies each metric by its **enforcement mode** (architectural recommendation from
the Usage Collector review):

- `QuotaGated` — usage of this metric **MUST** flow through Quota Enforcement before any Usage Collector record is
  emitted; the Usage Collector SDK rejects direct `emit()` calls for gated metrics. The integration wrapper performs the
  `preflight_reserve` → work → `settle` flow against Quota Enforcement; only on a successful settlement does the wrapper
  emit the corresponding Usage Collector record.
- `Direct` (non-gated) — usage of this metric is emitted directly via the Usage Collector with PDP-only authorization;
  Quota Enforcement is not consulted. Suitable for monitoring, audit, and analytics metrics that have no enforceable
  cap.

Quota Enforcement itself is agnostic to the enforcement-mode flag — it stores and evaluates Quotas for whatever metric
an operator declares. The flag determines **the path** by which usage records reach the Usage Collector, not whether
Quota Enforcement participates. In practice:

- Operators typically create Quotas only for `QuotaGated` metrics — Quotas on `Direct` metrics cannot constrain
  consumption.
- **Quotas on `Direct` metrics are inert.** Callers don't traverse QE for `Direct` metrics, so the counter would never
  reliably reflect consumption. Every write/preview operation (`debit`, `credit`, `rollback`, `reserve`, `commit`,
  `release`, batch-debit, `evaluate-preview`) targeting such a Quota **MUST** be rejected with `METRIC_NOT_QUOTA_GATED`
  — no counter mutation, no idempotency / operation-log / lease record. Quota Snapshot reads still succeed so operators
  can clean up. Create is permitted (a metric's mode can flip over time); while such a Quota stays active it is counted by the
  `quota_for_direct_metric_total` gauge.
- **A `QuotaGated` metric with no matching Quota for the operation's subject is rejected.** Quota Enforcement returns
  `Denied(violated_quota_ids=[], reason="NO_APPLICABLE_QUOTA")` — `QuotaGated` means every consumption requires explicit
  quota authorization, so absence of an applicable Quota is treated as absence of permission, not as unconstrained
  freedom. Operators are expected to provision Quotas (typically via subject-manager hooks at tenant/user creation)
  before any consumption can occur.

The detailed `preflight_reserve` → `settle` flow and the SDK rules that enforce the gating contract are **not** part of
this PRD and are not part of the Usage Collector specification either. They will live in a separate integration
component — either a dedicated QE↔UC wrapper authored alongside Usage Collector, or, in a later major release that
consolidates QE and UC responsibilities, as part of the merged surface. This PRD acknowledges the boundary so that QE's
primitives and vocabulary stay aligned with whichever shape the integration eventually takes.

### 3.3 Integration Boundary with Usage Collector

Quota Enforcement and the Usage Collector are intentionally decoupled:

- Quota Enforcement owns its own internal counters and ledger; it does not query Usage Collector at evaluation time.
- The Usage Collector owns the canonical record of consumption events; it does not know about quota caps or denials.

The integration component (out of scope for this PRD) is responsible for the `QuotaGated` flow
described in §3.2: it calls Quota Enforcement's `reserve`/`commit`/`release` (or `debit`/`rollback`) primitives, then —
on successful settlement — emits the corresponding Usage Collector record carrying a settlement proof that the Usage
Collector SDK validates before persisting the usage. For `Direct` metrics, the wrapper is not involved: callers emit
directly to the Usage Collector with PDP-only authorization. Operators who deploy Quota Enforcement without the wrapper
still receive a fully functional standalone counter/ledger system — calling services invoke Quota Enforcement's
primitives directly without Usage Collector involvement.

### 3.4 Decision Contract for Calling Services

Every Quota Enforcement evaluation operation returns one of two outcomes:

1. A deterministic **`Decision`** (a verdict produced by the active Quota Resolution Engine, §5.9), or
1. A platform-canonical **error** (when the system could not produce a Decision at all — Engine timeout, mid-flight
   infrastructure unavailability, malformed Engine output, invariant violation).

The `Decision` shape carries:

- `result` — one of:

  - `Allowed` — operation is permitted within every Quota's cap.
  - `Denied(violated_quota_ids, reason)` — at least one Quota would be exceeded; counters are not modified. Every
    violating Quota is named (no short-circuit).

- `debit_plan` — `Map<quota_id, QuotaDebitPlan>` naming exactly which Quotas to mutate and how. Each `QuotaDebitPlan`
  carries:

  - `amount` (≥ 0) — total counter mutation for that Quota.

  The struct is extension-ready: future per-Quota fields (e.g., a `clamped` marker if cap-clamp admission lands in a P3
  per §13 Open Questions) MAY be added without breaking the top-level Decision shape. Counters are mutated strictly per
  `debit_plan` when `result = Allowed`. `Denied` produces an empty `debit_plan` and no counter mutations.

- `diagnostics` — Engine-supplied per-Quota detail (current consumed, cap, contribution to the decision, Engine-specific
  notes) so callers can render breakdowns without recomputing Engine logic.

Built-in `most-restrictive-wins` Engine produces a single-entry `debit_plan` against the binding Quota at
`amount = request.amount`; Quotas not in the plan remain unchanged. A Quota is satisfiable if remaining ≥
`request.amount` (unbounded Quotas are trivially satisfiable). The binding Quota is selected from the satisfiable set by, in
priority order:

1. **Subject-scope tier** — more-specific tier wins (P1: user-scope > tenant-scope).
2. **Bounded > unbounded** within the chosen tier — operator's explicit cap is enforced first; unbounded acts as
   overflow when no bounded Quota in the tier is satisfiable.
3. **Smallest remaining** within bounded satisfiable Quotas of the chosen tier (literal "most restrictive"); ties broken
   by ascending `quota_id`. Among unbounded Quotas of the chosen tier (selected only when no bounded satisfiable exists
   in that tier), ascending `quota_id` is the sole tiebreaker.

`Denied` is returned when no Quota is satisfiable — every applicable bounded Quota has remaining capacity below
`request.amount` and no applicable unbounded Quota exists; `violated_quota_ids` enumerates every such bounded Quota.

Custom Engines (`cel` in P1; `starlark`/Wasm/etc. in P2 or later) MAY produce multi-entry `debit_plans` for cascade
splits, attribute-weighted distributions, and AND-across-tiers patterns; the per-entry Debit-Plan invariant
(`0 ≤ amount ≤ request.amount` per entry) applies uniformly at the Engine boundary — see
`cpt-cf-quota-enforcement-fr-quota-resolution-engine`.

**Failure surface.** When the Engine cannot produce a valid Decision — Engine timeout, Engine cost-cap exhaustion,
Engine internal failure, malformed Debit Plan, Debit-Plan invariant violation, mid-flight PDP / storage failure — the
operation **MUST NOT** mutate counters and **MUST** surface a platform-canonical error (per the AIP-193 categories
implemented by `cf-toolkit-canonical-errors`; specific category mapping is a DESIGN concern). These are operational
failures, not verdicts, and never appear as a `Decision` arm. A failure-shaped response is mutually exclusive with the
`Decision` shape — every evaluation operation returns either a `Decision` (`Allowed` / `Denied`) **or** a canonical
error, never both.

**Forward compatibility.** The two-arm `result` is a strict subset of any future expanded shape. A future P3
introduction of `AllowedWithClamp(quota_id, admitted_magnitude)`, intended for batch-style workloads, would be additive:
P1 consumers continue to handle only `Allowed` / `Denied`, and the would-be-clamped case is conservatively reported as
`Denied` until clamp mode is adopted.

**Trust boundary — Decision is server-derived, response-only.** The Decision (`result`, `debit_plan`, `diagnostics`) is
computed entirely server-side by the active Engine and returned to the caller for application of the verdict and for
diagnostic rendering. The Decision and every field within it **MUST NOT** be supplied, modified, or echoed back by the
caller — neither on the originating request nor on any subsequent request. Request DTOs do not carry Decision-shaped
fields by type design; if such fields appear in a request body (e.g., an over-eager client echoing back the response
shape), the server silently ignores them. This mirrors the authenticated-principal and PDP-authorized-attribution
discipline in §5.13. Replay safety is achieved exclusively via the client-supplied idempotency key (§5.8); on replay, the original
outcome (Decision or error) is the canonical response — never anything the client resubmits. This boundary prevents a
misbehaving or malicious caller from skipping a Quota's mutation, redirecting it to a different Quota, replaying a stale
plan against a now-different applicable-Quotas set, or otherwise bypassing Engine arbitration. Tokens that *do*
round-trip between client and server (idempotency keys, lease tokens, operation IDs) are opaque server-issued or
client-supplied identifiers — never structured Decision fields.

The calling service interprets the outcome according to its own policy: `Denied` is propagated to the user as
`429 Too Many Requests` or domain-specific equivalent; a canonical error triggers the calling service's fallback
behavior, which may be fail-open or fail-closed at the calling service's discretion (the canonical category — e.g.,
`DeadlineExceeded` vs `Internal` vs `ServiceUnavailable` — informs that choice). Quota Enforcement itself fails closed
on internal errors (surfaces a canonical error, never silently allows). **Commercial overage pricing** (a tenant exceeds
an entitled allowance and is charged premium rates for the overage) is **not modeled in the Decision contract**; per
§4.2, this is the billing service's responsibility, composed downstream of QE from Usage Collector observations and
Quota records.

## 4. Scope

### 4.1 In Scope

- Quota lifecycle (create, update, deactivate, read) — single first-class entity bound to a subject at creation time
- Declarative owner-scoped subject/resource projections, per-metric request contracts, attached constraint contracts, and fail-closed contract
  validation
- PDP-authorized caller attribution mapped to every supplied owner scope through the projection catalogue
- Allocation and consumption quota types (rate type declared as future)
- Calendar-aligned UTC time periods with deterministic rollover semantics
- Idempotent debit, credit, rollback operations
- Two-phase lease primitive (reserve/commit/release with TTL auto-release)
- Bulk / batch debit with envelope idempotency: atomic all-or-nothing for multi-metric admission (P1); partial-success
  for bulk-independent operations (P2)
- Multi-quota evaluation against the applicable-Quotas set
- Quota Resolution Policy as a separate operator-managed entity selecting a pluggable Quota Resolution Engine; default
  platform-wide policy uses the built-in `most-restrictive-wins` Engine; operator-configurable narrower-scope Policies
  may use any registered Engine
- Quota Resolution Engine plugin contract with P1 built-ins: `most-restrictive-wins` (hardcoded; fastest path) and `cel`
  (sandboxed; customizable). Cascade/spillover and attribute-gated arbitration are expressible via the customizable
  Engine in P1; additional Engines (Starlark, Wasm-loaded, …) plug in via the same contract in later phases without
  changes to the evaluation core
- Quota Snapshot read APIs (single subject, bulk subjects, end-user self-service)
- Quota enforcement mode; in P1 supports only strict rejection at the cap boundary; future modes are added as new GTS
  instances per `cpt-cf-quota-enforcement-fr-enforcement-mode`
- Tenant isolation on every read and write
- PDP-gated authorization on every operation
- Pluggable storage backend (P1: `toolkit-db`-based plugin; alternative backends viable under the same plugin contract)
- Notification plugin contract emitting the event catalog defined in `cpt-cf-quota-enforcement-fr-notification-plugin`
- Operational telemetry (gear-specific counters, histograms, and gauges; baseline observability via the platform
  framework)
- P1 ships a closed `source` enum on every Quota with values `licensing` (default; quotas materialized from the
  licensing layer) and `operator` (manual caps — incident response, compliance carve-outs, soft-launch placeholders).
  Both follow the same mutation rules: operator-level or Quota-Manager PDP scope. Future capability for
  tenant-administrator-imposed (`tenant_admin`) and end-user-self (`user_self`) Quotas with per-source mutation rules
  and cap-inheritance — together with subscriber-customizable notification thresholds — is a P2 enhancement; see §13.

### 4.2 Out of Scope

**Quota management and provisioning — umbrella delegation to Quota Manager.** Every concern in this group is **out of
scope for Quota Enforcement** and **owned by Quota Manager** (a separate platform component,
`cpt-cf-quota-enforcement-actor-quota-manager` in §2.2; its own PRD is authored independently). Quota Enforcement
exposes the primitives (Quota CRUD, debit/credit/rollback, lease, Quota Snapshot read, notifications) that Quota Manager
composes into management workflows. The boundary is normative: any new management/provisioning capability the platform
decides to ship belongs in Quota Manager unless and until a future PRD revision explicitly relocates it.

The umbrella covers:

- **License-pack catalog and plan templates** — operator definitions of named bundles ("Free", "Pro", "Enterprise"; "10
  GPU-hours / month / user"; "1 TB egress / tenant / month"; etc.) and the rules for materializing concrete Quotas from
  them. Quota Enforcement provides only the per-Quota CRUD primitive; bundling, templating, and plan-driven
  instantiation are Quota Manager's concern.
- **Subject-creation provisioning** — automatically attaching a plan to a freshly-created subject (tenant, user,
  cost-center, …) so that Quotas appear without operator hand-roll. Subject managers (e.g., `account-management`) signal
  subject lifecycle to Quota Manager; Quota Manager calls Quota Enforcement's CRUD on the resulting Quotas.
- **Redistribution / reallocation** — moving cap from one user to another within a tenant, splitting a tenant top-up
  across users, draining a deactivated user's allocation back into the tenant pool, etc. Composed in Quota Manager from
  existing Quota Enforcement update/credit primitives; Quota Enforcement does not introduce a "redistribute" verb.
- **Quota increase-request workflow** — self-service raise + operator approval lifecycle, including the request entity,
  the approval state machine, the cap-bump audit trail, and the human-facing surfaces. Owned by Quota Manager (replaces
  the earlier "deferred / wrapper component or workflow gear" framing — see §13).
- **License purchase / renewal / expiry / revocation hooks** — translating commercial events into Quota lifecycle
  changes (create, update cap, deactivate, reactivate). Owned by Quota Manager; Quota Enforcement merely receives the
  resulting CRUD calls.
- **Tenant-administrator UI surfaces** — assigning caps within a tenant, reviewing utilization, reacting to threshold
  notifications, and configuring per-user caps within operator-imposed bounds. Owned by Quota Manager.
- **End-user UI surfaces** — self-service Quota Snapshot rendering, "explain why this was denied" explanations,
  request-to-raise affordances. Owned by Quota Manager; Quota Enforcement provides only the underlying snapshot-read API
  consumed by Quota Manager (`cpt-cf-quota-enforcement-fr-end-user-quota-snapshot-read`).
- **Pricing, rating, billing rules, invoice generation** — quota counters are dimensionless integer/decimal values, not
  currency amounts. Owned by the billing layer (separate from Quota Manager and from Quota Enforcement).
- **Cost-based quotas in monetary units** — P1 Quotas count metric units (tokens, bytes, hours), not cost.
  Cost-translated caps, if needed, are computed in Quota Manager / billing and materialized as metric-unit Quotas in
  Quota Enforcement.
- **Commercial overage and overage pricing** — when a tenant's premium product permits consumption above an entitled
  allowance and is charged premium rates for the overage, this is the **billing service's** responsibility, composed
  downstream of QE from Usage Collector observations and Quota records. QE itself does not admit operations above cap;
  operators wanting "smooth" premium-tier UX express it by raising the cap (or setting `cap = null` for an unbounded
  Quota per `cpt-cf-quota-enforcement-fr-quota-lifecycle`) on premium tenants and configuring the billing service to
  apply per-unit pricing above an entitled allowance. This division keeps the QE admission contract simple and
  concentrates pricing logic in the billing layer where catalog, currency, taxation, and tier definitions already live.
- **Operator approval workflows** — multi-step approvals for cap raises, plan migrations, redistribution between
  subjects, etc. Owned by Quota Manager.
- **Plan migration / customer-tier change** — moving a tenant from "Free" to "Pro" and rewriting its Quotas accordingly.
  Owned by Quota Manager.

The remaining out-of-scope items below are not management/provisioning but distinct exclusions retained for
completeness:

- **Usage Collector integration** — handled by a separate wrapper component (out of scope).
- **Cross-tenant quotas** — every quota is tenant-scoped; cross-tenant arrangements are explicitly excluded.
- **User-facing UI** — only REST and SDK surfaces are exposed by Quota Enforcement itself; all UIs (operator, admin,
  end-user) are the responsibility of consuming products and Quota Manager.
- **Audit logging** (consumption events and definition changes) — deferred to P2 when the platform audit infrastructure
  is available.
- **Subject hierarchy via resource-group** — P3 only; P1 supports `tenant` and `user` directly without traversal.
- **Breaking projection-version activation** — out of P1 per `cpt-cf-quota-enforcement-fr-quota-lifecycle`: the
  current projection version stays active, replacement contracts may be registered but are rejected by Quota and
  Policy writes with `PROJECTION_NOT_RESOLVABLE`, and P1 provides no projection alias or Quota/counter migration
  operation.
- **Grace policies for hard caps** (e.g., +N% for T minutes during a campaign or incident before resuming the strict
  cap) — deferred to P2.
- **Rate quota implementation** (token bucket / sliding window) — declared as a future quota type; P1 implements only
  allocation and consumption.
- **EventBus integration** — P1 emits events through the in-process notification plugin; standardized EventBus routing
  is P2.
- **Multi-region replication** — single-region deployment; multi-region deferred (target resolution: production demand).
- **Per-resource counter axis** — resource projections are in scope as a typed selection contract, but resource does not
  enter the P1 counter key; §13 records the decision criterion for adding that axis.
- **Derived or attested attribution** — a future wrapper may construct the same raw attribution tuple from forwarded
  user context once its authentication and trust model are defined.
- **Business-semantic property validation** — contracts validate shape; acceptable business values and their meaning
  remain with the Engine/Policy layer.

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting 90%+ code
> coverage unless otherwise specified.

### 5.1 Projection Contracts & Subject Attribution

#### Projection Contracts

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-projection-contracts`

QE **MUST** register abstract subject, resource, request, and constraint bases at `gts.cf.core.qe.subj.v1~`,
`gts.cf.core.qe.res.v1~`, `gts.cf.core.qe.request.v1~`, and `gts.cf.core.qe.constraint.v1~`. It **MUST** also register the scope-discriminator type
`gts.cf.core.qe.scope.v1~` and its P1 `user` and `tenant` well-known instances. The Gear that owns a metric
**MUST** publish concrete derived projections for every supported subject scope. Callers **MUST NOT** select a
projection or create per-caller projections for the same metric. QE maps each caller-supplied `(metric, kind)` pair
through its validated catalogue to obtain the projection type used by applicable-Quota lookup:
`(projection_type, subject_id, metric)`.

Each subject projection **MUST** declare a required `scope` trait in the base's `x-gts-traits-schema`. Its value
**MUST** be a `GtsInstanceId` narrowed to `gts.cf.core.qe.scope.v1~*`; the P1 values are
`gts.cf.core.qe.scope.v1~cf.core.qe.user.v1` and
`gts.cf.core.qe.scope.v1~cf.core.qe.tenant.v1`. Scope instances are identity-only discriminators; they do not name a
`SecurityContext` accessor. QE **MUST** read the registry-validated effective trait and compare instance ids directly.
Scope **MUST NOT** be inferred from the type-id name segment.

Each projection **MUST** declare its admitted metrics through a typed
`x-gts-traits` value whose entries are metric instance ids narrowed by `x-gts-ref`. `x-gts-ref` is pattern-level only: it
validates that a value is a well-formed GTS id under the declared prefix. The system **MUST** therefore itself verify,
at bootstrap per `cpt-cf-quota-enforcement-fr-contract-validation`, that each referenced metric is registered and is
genuinely an instance of the metric base. A narrowed `x-gts-ref` is a prefix match, not an instance-of check. The request
may also carry a resource projection for typed resource properties; resource does not enter the P1 counter key. Metric
identity remains owned by `types-registry` and is not changed by this requirement.

Within a deployment, a metric **MUST** be admitted by at most one concrete subject projection **per `(metric, scope)`
pair**. One owner may own several metrics and one projection may admit several, so the constraint binds the pair.
Two projections admitting the same metric at the same scope split the applicable-Quota key and fragment one budget into
two counters — the precise failure owner-declared projections exist to prevent. QE **MUST** reject that configuration at
bootstrap rather than resolving it by precedence: a silent winner would make which counter is debited depend on
configuration order. Distinct scopes of the same owner (for example `user` and `tenant`) admitting the same metric is
the intended arrangement and **MUST NOT** be rejected — it is what makes scope cascade
(`cpt-cf-quota-enforcement-fr-quota-cascade`) expressible.

For each metric, the owner **MUST** publish exactly one concrete request contract derived from
`gts.cf.core.qe.request.v1~`. Its required traits identify the single `metric: GtsInstanceId` and attach a concrete
`constraint_contract: GtsTypeId` derived from `gts.cf.core.qe.constraint.v1~`. The request contract refines the one
operation-level `metadata` object shared by all callers and subject scopes. The request schema **MUST NOT** be reused
for arbitration constraints because the two sides may intentionally differ in shape and cardinality.
Contract registration occurs through `types-registry`; this requirement adds no QE endpoint.

- **Rationale**: Owner-declared identity preserves one cross-service counter and turns the registry into a reviewable
  inventory of metered subjects, resources, properties, and admitted metrics.
- **Actors**: `cpt-cf-quota-enforcement-actor-metric-owner`,
  `cpt-cf-quota-enforcement-actor-quota-consumer`, `cpt-cf-quota-enforcement-actor-types-registry`

#### Contract Validation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-contract-validation`

At Quota create/update, QE **MUST** resolve the constraint contract attached to the metric request contract and validate
`quota.metadata` before persistence. It **MUST NOT** revalidate stored metadata on the evaluation path. At ingress of
every subject-based consumer evaluation operation, including each batch item, QE **MUST** validate `tenant_id`, every
`subjects[{kind,id}]` entry, the required operation-level `metadata`, the optional resource projection, and the metric
before Engine dispatch. Missing, duplicate, unknown, or unadmitted subject kinds; absent metadata; or schema violations
**MUST** receive a canonical `InvalidArgument` error. QE **MUST NOT** default absent metadata to `{}` and **MUST NOT**
encode validation failure as `Decision::Denied`.

QE **MUST** validate operation metadata, the optional resource projection, and Quota metadata against their complete
resolved contracts, including the base contract rules. The subject family declares traits only and carries no
`metadata`. ADR-0007 defines the validation mechanics.

Contract schema resolution **MUST NOT** add a live `types-registry` dependency to evaluation. Quota and Policy writes
resolve and snapshot contracts outside storage transactions; the hot path consumes those snapshots. Contracts validate
shape only and must not contain secrets. Compatibility follows ADR-0007: adding an optional property is non-breaking;
remove, rename, retype, narrow, add a required property, or narrow admitted metrics requires a new version.

- **Rationale**: Enforced contracts eliminate silent attribute misses while keeping registry latency and availability
  outside the evaluation budget.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`,
  `cpt-cf-quota-enforcement-actor-platform-operator`

#### Projection Catalogue Consistency

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-subject-type-registry`

The subject catalogue is the configured set of concrete owner projections in `types-registry`; QE has no subject-type
table or resolver trait. Platform-wide seeded `tenant` and `user` subject types are replaced by owner-scoped projection types such as
`gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.tenant.v1~` and
`gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~`. QE **MUST** reject Quota creation against an unregistered,
abstract, non-subject, unknown-scope, or non-configured projection, or one that does not admit the Quota's metric.

Bootstrap **MUST** validate every admitted metric reference, enforce uniqueness of `(metric, scope)`, resolve exactly
one concrete request contract per admitted metric, and validate its attached constraint contract. It **MUST** also fail
when the configured catalogue is incompatible with any active Quota or Policy.

- **Rationale**: Registry-resident owner projections provide discoverable identity and shape while a validated local
  catalogue makes caller attribution deterministic and auditable.
- **Actors**: `cpt-cf-quota-enforcement-actor-metric-owner`

#### Subject Attribution at Evaluation Time

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-subject-resolution`

Every subject-based consumer evaluation request **MUST** carry `tenant_id`, `subjects: [{kind, id}]`, and one operation-level `metadata` object.
The tenant scope is materialized from `tenant_id`; `subjects` carries additional applicable scopes and **MUST NOT**
repeat the tenant scope. `id` is an opaque non-empty string. QE **MUST** first reject malformed public request shape:
missing required fields, wrong public container types, empty ids, duplicate kinds, or tenant scope repeated in
`subjects`. It **MUST** then ask PDP to authorize the complete
caller-supplied tenant/subject/metric/resource tuple against the authenticated service principal. Only after authorization
may QE establish that `kind` is a registered, admitted QE scope instance or resolve any catalogue/contract information.

QE **MUST** map every authorized `(metric, kind)` through `ProjectionContractCatalog`, fetch every active Quota for the
resulting `(projection_type, subject_id)` set, and forward the complete set to multi-quota evaluation. Missing,
duplicate, unknown, or unadmitted kinds fail before evaluation; callers cannot select or omit a concrete projection for
a supplied scope. An authorized operation with no matching Quotas is evaluated against an empty set and returns
`Denied(violated_quota_ids=[], reason="NO_APPLICABLE_QUOTA")` per §3.2.

- **Rationale**: The raw S2S API matches Usage Collector's attribution model while PDP authorization prevents a caller
  from charging another tenant or subject. Catalogue mapping keeps concrete projection selection server-owned.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

### 5.2 Quota Lifecycle

#### Quota Lifecycle: Create, Update, Deactivate, Read

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-lifecycle`

The system **MUST** support a declarative lifecycle for Quotas: create, update, deactivate, and read. A Quota is a
single first-class entity that is bound to its subject at creation time; there is no separate definition-template or
assignment-binding concept. Each Quota **MUST** carry:

- a unique stable quota ID (server-assigned),
- a subject reference `(projection_type, subject_id)` — the registered owner projection plus opaque subject id,
  bound at creation and immutable for the lifetime of the Quota,
- a reference to a metric name registered in `types-registry` (validated as described in
  `cpt-cf-quota-enforcement-fr-metric-identity-validation`),
- a quota type (`allocation` or `consumption`; `rate` is reserved for future use and is rejected at creation time in P1
  per `cpt-cf-quota-enforcement-fr-quota-type-rate-rejection`),
- a period specification: consumption types **MUST** carry an explicit valid period, the non-recurring `one_time`
  included, and a missing or null period is rejected (`PERIOD_REQUIRED`); allocation types **MUST** reject any
  period field, a null period included (`PERIOD_NOT_ALLOWED`),
- an `enforcement_mode` value (the Quota's behavior at the cap boundary); in P1 only strict rejection at the cap
  boundary is supported; future modes are added as new GTS instances per `cpt-cf-quota-enforcement-fr-enforcement-mode`,
- a cap (units defined by the metric; either a non-negative integer or `null` for an **unbounded** Quota; negative caps
  are rejected at create/update with `CAP_MUST_BE_NON_NEGATIVE`; `cap = 0` and `cap = null` are both explicitly valid —
  see "Cap value semantics" below). All P1 metric values are integer; metric authors registering inherently fractional
  concepts (e.g., currency, fractional CPU-hours) **MUST** denormalize to a sufficiently small integer unit (cents,
  milliCPU-seconds) before registration in `types-registry`,
- an optional set of notification thresholds expressed as percentages of cap (e.g., `[50, 80, 100]`); thresholds are
  rejected at create/update on unbounded Quotas (`THRESHOLDS_REQUIRE_BOUNDED_CAP`) — percentages of `null` are
  meaningless,
- an optional validity window (start and end timestamps; when absent, the Quota has no time bounds and remains evaluable
  until explicitly deactivated),
- an optional failure-mode hint (`fail-closed` default, `fail-open` opt-in) — informational metadata for callers,
- an optional `metadata` JSON object (string keys, arbitrary JSON values; constraints defined by
  `cpt-cf-quota-enforcement-fr-quota-metadata`) — operator-supplied attributes opaque to QE core, surfaced to the active
  Engine via EvaluationContext for attribute-based selection (region, tier, environment, …),
- a `source` value identifying who imposed the Quota; see **Source value semantics** below.

The system **MUST** reject creation of a Quota whose projection is not registered, concrete, derived from the QE subject
base, is absent from the configured evaluation catalogue, or does not admit the metric, and **MUST** reject a
`subject_id` that violates the declared scope.

**Source value semantics.** `source` instances live under base `gts.cf.qe.source.type.v1~`. P1 seeds two:
`gts.cf.qe.source.type.v1~cf.qe.source.licensing.v1` (default; caps materialized from the licensing layer) and
`gts.cf.qe.source.type.v1~cf.qe.source.operator.v1` (caps an operator created manually outside the licensing
flow — incident response, compliance carve-outs, soft-launch placeholders). A stored Quota's `source` value never
changes silently. Mutation rules in P1 are uniform across both source values (operator-level or Quota-Manager PDP
scope; see the mutation paragraph below); P2 source-kind extensions (`tenant_admin`, `user_self`, …) and per-source
cap-inheritance and mutation rules land with the P2 Source Registry FRs tracked in §13.

> **Notation.** Throughout this document, source values are referenced by their short instance names (`licensing`,
> `operator`) in examples and use-case payloads for readability. API requests, storage rows, and outbox events
> use the full GTS URI form shown above.

**Multiple Quotas per `(subject, metric)` are permitted.** The system **MUST NOT** reject Quota creation on the basis
that another active Quota already exists for the same `(subject, metric)` pair. When several Quotas share the same
`(subject, metric)`, all of them enter the applicable-Quotas set at evaluation time and are resolved through multi-quota
evaluation (`cpt-cf-quota-enforcement-fr-multi-quota-evaluation`) under the active Quota Resolution Policy — by default
`most-restrictive-wins` (per `cpt-cf-quota-enforcement-fr-quota-resolution-engine`). Typical reasons operators
legitimately create more than one Quota per `(subject, metric)`:

- attribute-gated arbitration (per-region, per-tier, per-environment caps differentiated via Quota Metadata, evaluated
  by a `cel` Policy per `cpt-cf-quota-enforcement-fr-attribute-based-quota-selection`),
- promotional or campaign-window caps living alongside steady-state caps (resolved through validity windows and Engine
  logic),
- license composition where multiple entitlements contribute caps to the same metric (operator-managed: see §4.2 —
  composition into a final cap is the licensing/billing layer's concern; QE simply hosts the resulting Quota records).

Operators (and Quota Manager flows acting on their behalf) are responsible for ensuring the declared set of Quotas
matches the intended arbitration shape — accidentally duplicate or contradictory Quotas are not rejected by QE; their
effect is observable through evaluation outcomes and the per-Quota detail surfaced in Decision diagnostics.

Operators who need the same shape on many subjects create one Quota per subject (typically driven by the subject manager
at subject-creation time); bulk template creation is out of scope (see §4.2).

Quota deactivation **MUST** retain the Quota record for read access but **MUST** stop accepting new debits or leases
against it. Active leases against the deactivated Quota **MUST** be marked as * *resolved-by-deactivation*\* atomically
with the deactivation transaction; subsequent `commit` or `release` calls against those leases **MUST** return
`LEASE_NOT_ACTIVE`. The deactivation timestamp serves as the implicit lease-resolve event for telemetry (no held
capacity remains against the deactivated Quota's counter); a `lease-resolved-by-deactivation` notification event (one of
the catalog kinds in `cpt-cf-quota-enforcement-fr-notification-plugin`) **MUST** be emitted for each affected lease with
the lease ID, owning subject context, held amount, and the deactivated `quota_id`. Updates **MUST** preserve the quota
ID and the subject reference; breaking changes (changing metric, type, period, or subject) **MUST** be performed by
deactivating the original Quota and creating a new one.

Activating a breaking projection-version change is **out of scope for P1**. The current version remains active. A
replacement contract may be registered, but Quota and Policy writes referencing it and deployments whose configured
catalogue is incompatible with any active Quota or Policy **MUST** be rejected; writes return
`PROJECTION_NOT_RESOLVABLE`. P1 provides no projection alias or Quota/counter migration operation. Any future activation
mechanism **MUST** also satisfy the catalogue/retry compatibility rule in §5.8.

**Cap reductions guard.** A Quota update that would reduce `cap` strictly below the Quota's current consumed amount (
consumption-type Quotas, within the active period) or current in-flight count (allocation-type Quotas) **MUST** be
rejected with an actionable `CAP_BELOW_CONSUMED` error. Operators wanting to reduce a cap below current usage **MUST**
first issue credits (`cpt-cf-quota-enforcement-fr-credit`) to bring consumption to or below the desired cap, then reduce
the cap. Cap raises (including any update from numeric → `null`) and updates where the new cap is greater than or equal
to current usage are not affected by this guard. Updates from `null` → numeric are subject to the same guard against the
current consumed amount. The cap-vs-consumed comparison **MUST** be evaluated at the moment the update transaction
commits, not at request-receipt time, to avoid TOCTOU races with concurrent debits.

**Cap value semantics.** Two boundary cases are explicitly valid:

- `cap = 0`: a `hard` Quota with `cap = 0` denies every debit against it (any non-zero amount exceeds remaining = 0).
  Useful as an explicit "deny everything for this `(subject, metric)`" Quota — placeholder created at subject
  provisioning before the operator decides on a real cap, or a deliberate cutoff during an incident response.
- `cap = null` (**unbounded**): the Quota imposes no upper bound. It is **always satisfiable** — every applicable-set
  evaluation that would otherwise debit against it produces a non-violating contribution and the operation proceeds.
  Counters still increment on debit/commit (so usage telemetry, audit trail, and a later migration to a real numeric cap
  all work without backfill); `remaining` is reported as `null` in Quota Snapshots, indicating "no upper bound". For
  consumption-type unbounded Quotas, period rollover (`cpt-cf-quota-enforcement-fr-period-rollover`) applies uniformly
  with bounded Quotas: `consumed` resets to zero at the period boundary and a `period-rollover` event is emitted
  carrying the closing-period `consumed` (closing-period `cap` is `null`); `remaining` stays `null` across the boundary.
  Long-term cumulative telemetry is reconstructed by downstream consumers from the rollover events, not from the live
  counter. Useful for premium-tenant "no commercial cap", internal/platform subjects, soft-launching a `QuotaGated`
  metric before per-tenant caps are configured, and bridging cap-bump approval workflows. Notification thresholds are
  not allowed on unbounded Quotas (per the field constraints above) — percentages of `null` are meaningless.

The system **MUST** surface the telemetry gauges `quota_cap_zero_total` and `quota_cap_unbounded_total` counting the
Quotas with lifecycle status `active` whose `cap = 0` and `cap = null` respectively, whatever their validity window. The system **MUST NOT** auto-reject either value — operators
are responsible for the choice.

**Validity-window semantics — Engine-driven, with a default exclusion.** A Quota's `validity_window` is a structural
field exposed in the Engine EvaluationContext (`cpt-cf-quota-enforcement-fr-quota-resolution-engine`); the Quota
Enforcement core does **NOT** auto-deactivate a Quota when `now() > validity_end`. Lifecycle (active vs deactivated) and
validity-window bounds are independent dimensions:

- The Quota record persists with its `validity_window` until explicitly deactivated (per §5.2 deactivation rule;
  hard-delete of deactivated Quotas is a P2 candidate per §6.2 / §13).
- At evaluation time, the active Engine sees the Quota's `validity_window` alongside `time`; the **default** behavior of
  the built-in `most-restrictive-wins` Engine is to **exclude** Quotas whose `time` falls outside
  `[validity_start, validity_end]` (i.e., not-yet-valid or expired Quotas are skipped from the Debit Plan).
- Operator-authored Policies (via the `cel` Engine in P1) **MAY** override this default — for example, to implement
  tier-conditional cutoffs or expected-window matching against `request.expected_window` (the canonical
  client-supplied attribute for "I'm operating as-of this time window; only consider Quotas valid for it"). The PRD does
  not mandate any single behavior — the choice is Engine-side.
- Active leases acquired within a Quota's valid window remain honorable across the validity boundary; see
  `cpt-cf-quota-enforcement-fr-lease-commit` for cross-boundary commit semantics.
- Quota Snapshot read APIs (`cpt-cf-quota-enforcement-fr-quota-snapshot-read`,
  `cpt-cf-quota-enforcement-fr-end-user-quota-snapshot-read`) **MUST** surface `validity_window` and a server-computed
  boolean `currently_within_window` so callers can render expiry state without recomputing the comparison.

This treatment keeps validity-window flexible: operators who want hard cutoff get it for free (the default exclusion),
operators who want grace or conditional behavior author a Policy, and callers who need window-aware evaluation pass
`expected_window` in request metadata.

In P1, Quota mutation requires operator-level or Quota-Manager PDP scope, regardless of `source` value. Both
`source = licensing` (the common case, materialized by the licensing layer) and `source = operator` (manual caps —
incident response, compliance carve-outs, soft-launch placeholders) follow the same mutation rules in P1; the `source`
field is recorded for forward-compatibility with the P2 Source Registry rather than for differential authorization.
Quota Manager (acting on behalf of tenant administrators or the licensing layer) **MAY** drive Quota CRUD only within
the original caller's tenant scope (PDP-enforced via the propagated SecurityContext); cross-tenant operations **MUST**
be rejected. P2 introduces additional source kinds (`tenant_admin`, `user_self`) with per-source mutation rules and
cap-inheritance constraints; see §13 Open Questions. Threshold notifications in P1 use the Quota's
`notification_thresholds` field directly per `cpt-cf-quota-enforcement-fr-notification-plugin`; P2 introduces
subscriber-customizable thresholds via an auto-created Subscription overlay (see §13) so subscribers can disable noisy
default events or add their own thresholds without modifying the Quota.

- **Rationale**: A single first-class Quota entity removes the conceptual ambiguity of a template-plus-binding split
  when templates are explicitly delegated to subject managers — operators reason about one entity bound to a subject at
  creation time instead of two interlocking records. Validity windows enable scheduled cap changes (e.g., promotional
  limits during a campaign) without ad-hoc rewrites.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-manager`

#### Metric Identity Validation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-metric-identity-validation`

The system **MUST** validate at Quota creation and update time that the referenced metric name exists in the platform
`types-registry`. An unknown metric **MUST** be reported as an actionable creation-time error. If the registry is
unreachable, Quota creation/update **MUST** fail with an actionable error. Persisted Quotas whose metric is later
removed from `types-registry` **MUST** be flagged via operational telemetry but **MUST NOT** be auto-deactivated.

- **Rationale**: Catching typos and stale references at creation time prevents silently inert Quotas. Failing fast when
  the registry is unreachable avoids silently accepting unverifiable metric references.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-types-registry`

#### Quota Metadata

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-metadata`

The system **MUST** support an operator-authored `metadata` object on every Quota, attached at create or update time and
validated against the constraint contract attached to the metric request contract per
`cpt-cf-quota-enforcement-fr-contract-validation`. Quota Metadata remains semantically **opaque to QE core**: QE
**MUST NOT** interpret business meaning and **MUST NOT** index metadata for direct query. Validated metadata is stored
and forwarded verbatim to the active Quota Resolution
Engine as the current Quota's `arbitration` object (stored as `Quota.metadata`; see
`cpt-cf-quota-enforcement-fr-quota-resolution-engine`); Engines and their Policies MAY use it to filter applicable
Quotas, gate evaluation on the validated `request` object, or feed Engine-specific arbitration logic (
`cpt-cf-quota-enforcement-fr-attribute-based-quota-selection`).

The system **MUST** enforce a single operator-configurable size limit and reject Quota create/update at validation time
when violated:

- maximum total serialized metadata size (canonical JSON): **4 KB** per Quota (default).

The owner-published contract defines keys, requiredness, types, enums, and nesting. QE validates it at Quota
create/update only; a stored value is not revalidated during evaluation. Shape enforcement does not transfer semantic
ownership to QE core: the active Engine/Policy still decides what valid property values mean.

Metadata is mutable as part of standard Quota updates; metadata changes **MUST NOT** invalidate the Quota's identity (
the quota_id is stable across metadata updates) but **MUST** emit a `quota-changed` notification event so sinks can
react. The full metadata object **MUST** be returned by every Quota read API (single-Quota, bulk, end-user self-service
— subject to PDP scoping) so callers can inspect the operator's gating intent.

Quota Metadata **MUST NOT** carry PII or other regulated data — it is classified as Platform Operational Data per §6.2,
visible to platform operators and (subject to PDP scope) Quota Manager acting on behalf of tenant administrators.
Operators are responsible for ensuring metadata content respects this classification; the system **MUST** surface
telemetry on metadata size distribution so operators can detect drift.

- **Rationale**: Attribute-based Quota selection (region-gated caps, tier-gated caps, weight-based proportional split,
  environment-gated caps) is a routinely-requested pattern. JSON-valued metadata lets operators express native types
  (e.g., `weight: 4`, `enabled: true`, `regions: ["us-east-1", "us-west-2"]`) without coercing through stringified
  representations and matching `int(...)` / `bool(...)` calls in every CEL Policy. Modeling these as opaque per-Quota
  metadata keeps the QE core attribute-agnostic — the matching logic lives in the Policy/Engine layer — while still
  letting operators express rich gating without code changes or new structural Quota fields. The 4 KB byte-size limit is
  operator-configurable so deployments can tighten the bound when threat models demand. The opacity rule prevents the QE
  core from becoming a half-baked attribute-query engine; if direct metadata-indexed queries are ever needed ( P2 or
  later), they will be added explicitly rather than emerging accidentally from query patterns. The separate contract
  permits intended request/arbitration differences such as request `region: string` paired with arbitration `regions: string[]`.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-manager`

#### Bulk Quota CRUD (P2)

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-fr-bulk-quota-crud`

The system **MUST** provide transactional bulk Quota CRUD endpoints for Quota Manager workflows that materialize
multiple Quotas from a single logical event (license-pack provisioning, plan migration, redistribution batches, tenant
offboarding):

- `bulk_create_quotas([Q1, Q2, …])` — atomically create every listed Quota, or none. Partial failure rolls back the
  entire batch.
- `bulk_update_quotas([{id, patch}, …])` — atomically apply every patch, or none.
- `bulk_deactivate_quotas([id, id, …])` — atomically deactivate every listed Quota, or none. Each affected Quota's
  active leases are resolved-by-deactivation per `cpt-cf-quota-enforcement-fr-quota-lifecycle` deactivation rules; the
  entire batch's lease-resolution events are emitted atomically with the deactivation transaction.

Each bulk operation **MUST** carry a single envelope idempotency key; replay returns the original outcome without
re-applying. Per-item idempotency keys MAY also be supplied for individual identification. The system **MUST** enforce a
configurable maximum batch size (default: **50** items per batch) and reject oversized batches with an actionable
`BULK_TOO_LARGE` error. Failures **MUST** identify the offending item(s) by index and reason so the caller can retry
with corrections; partial-success is not a permitted outcome — the entire batch either commits or rolls back. Bulk
operations **MUST** be subject to the same PDP authorization, tenant-isolation, and trust-boundary rules as their
single-item counterparts (per `cpt-cf-quota-enforcement-fr-authorization`,
`cpt-cf-quota-enforcement-fr-tenant-isolation`, §3.4 trust boundary).

- **Rationale**: Quota Manager workflows currently must compose per-Quota CRUD calls and carry their own compensation
  logic for partial failures (license-pack create halfway through, plan migration halfway through). Pushing
  transactional atomicity into Quota Enforcement, where the storage plugin already exposes the transactional primitives
  (per `cpt-cf-quota-enforcement-contract-storage-plugin`), simplifies Quota Manager and eliminates half-applied batches
  under failure. P2 priority because P1 license-pack flows can be served by per-Quota CRUD with operator-side
  compensation; transactional atomicity is a quality-of-life and reliability improvement, not a P1 blocker. Maximum
  batch size 50 is conservative — smaller batches simplify error attribution and reduce the blast radius of a
  misconfigured caller.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-manager`, `cpt-cf-quota-enforcement-actor-platform-operator`

### 5.3 Quota Type Semantics

Each Quota carries a `quota_type` identifying its accounting model. P1 reserves three GTS instances under base
`gts.cf.qe.quota.type.v1~`:

- `gts.cf.qe.quota.type.v1~cf.qe.quota.allocation.v1` — in-flight reservable capacity (no period reset).
- `gts.cf.qe.quota.type.v1~cf.qe.quota.consumption.v1` — per-period cumulative consumption (resets at the period
  boundary).
- `gts.cf.qe.quota.type.v1~cf.qe.quota.rate.v1` — P3 type; Quota creation is rejected in P1 with
  `NOT_YET_IMPLEMENTED` per `cpt-cf-quota-enforcement-fr-quota-type-rate-rejection`.

> **Notation.** Throughout this document, quota types are referenced by their short instance names (`allocation`,
> `consumption`, `rate`) in examples and use-case payloads for readability. API requests, storage rows, and
> outbox events use the full GTS URI form shown above.

#### Allocation Quota Semantics

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-type-allocation`

The system **MUST** support allocation-type Quotas that bound in-flight reservable capacity. An allocation Quota
maintains a single counter representing currently-in-use capacity for that Quota. The counter increments on debit and
on lease acquire (the lease reserves capacity for its TTL); it decrements on credit, on lease release / TTL
auto-release, on rollback, and on lease commit by the unused `reserved − actual` portion. Allocation Quotas **MUST
NOT** carry a period; the counter persists across calendar boundaries until explicitly modified by an operation.

- **Rationale**: Allocation quotas model concurrent-use limits (e.g., maximum running VMs per tenant, maximum open file
  descriptors per user) where the cap is a point-in-time invariant rather than a per-period budget.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Consumption Quota Semantics

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-type-consumption`

The system **MUST** support consumption-type Quotas that bound cumulative consumption within a recurring time period. A
consumption Quota maintains a per-period counter representing consumed amount within the current period. The counter
increases on debit and on lease acquire (the lease reserves capacity for its TTL, attributed to the acquisition period
per `cpt-cf-quota-enforcement-fr-lease-commit` cross-period rules); it decreases on credit, on lease release / TTL
auto-release, on rollback, and on lease commit by the unused `reserved − actual` portion. The counter **MUST** reset
to zero at the period boundary per `cpt-cf-quota-enforcement-fr-period-rollover`.

- **Rationale**: Consumption quotas model per-period budgets (e.g., 10 000 AI tokens per day, 1 TB egress per month)
  where the cap is the maximum amount consumable within a recurring window.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Rate Quota Type — P1 Rejection

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-type-rate-rejection`

The system **MUST** declare `rate` as a reserved quota-type identifier and **MUST** reject Quota creation requests with
`type=rate` in P1 with an actionable "not yet implemented" error. The data model and API surface **MUST** leave room for
adding rate semantics in P3 without breaking changes to existing Quotas; persisted Quotas with
`type ∈ {allocation, consumption}` **MUST NOT** require migration when the `rate` type is later activated.

- **Rationale**: Reserving the identifier and the data-model slot in P1 prevents naming collisions and
  forwards-compatibility breaks when rate-style smoothing is introduced. The reject behaviour is itself a P1 normative
  obligation and lives separately from the P3 implementation contract
  (`cpt-cf-quota-enforcement-fr-quota-type-rate-declared`).
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`

#### Rate Quota Type — P3 Implementation Contract

- [ ] `p3` - **ID**: `cpt-cf-quota-enforcement-fr-quota-type-rate-declared`

**P3 contract — token-bucket admission with burst.** P3 implementation will model rate Quotas with token-bucket
admission semantics carrying:

- `rate` — the sustained admission rate (e.g., 100 ops / minute);
- `burst_capacity` — the bucket size; up to this many operations are admitted without backoff if the bucket is full at
  request time;
- `smoothing_window` — the period over which the bucket refills at `rate` (typically equal to or smaller than the rate's
  denominator).

Admission against an exhausted rate Quota **MUST** return `Denied(reason = "RATE_WINDOW_EXHAUSTED")` with a
`Retry-After` floor (advisory; clients SHOULD add randomized jitter). Cap-clamp does not apply to rate Quotas — the only
valid admission outcomes for `quota_type = rate` remain `Allowed` / `Denied` (with operational failures surfaced as
canonical errors). Burst implementation choices ( sliding-window vs token-bucket vs fixed-window; per-tenant vs
per-region smoothing axis) are tracked in §13 Open Questions and resolved when P3 implementation begins. The P1 reject
contract from `cpt-cf-quota-enforcement-fr-quota-type-rate-rejection` is superseded once this FR is implemented; until
then, `type=rate` creation requests continue to be rejected per the P1 FR.

- **Rationale**: Enumerating the future contract shape now prevents P3 implementation from re-deriving cap and bucket
  semantics from scratch.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`

### 5.4 Time Period & Reset Semantics

#### Period Specification & Calendar Alignment

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-period-semantics`

The system **MUST** support consumption-quota periods drawn from a fixed set of GTS instances under base
`gts.cf.qe.period.type.v1~`. P1 reserves five instances:
`gts.cf.qe.period.type.v1~cf.qe.period.day.v1`,
`gts.cf.qe.period.type.v1~cf.qe.period.week.v1`,
`gts.cf.qe.period.type.v1~cf.qe.period.month.v1`,
`gts.cf.qe.period.type.v1~cf.qe.period.year.v1`, and
`gts.cf.qe.period.type.v1~cf.qe.period.one_time.v1`. All periods **MUST** be UTC and calendar-aligned by
default: a `day` period begins at 00:00 UTC and ends at 24:00 UTC; a `month` period begins at 00:00 UTC on the first
day of the calendar month; a `week` period begins at 00:00 UTC on Monday. The `one-time` period denotes a non-recurring
quota (typically used for promotional or single-event budgets) — it has no automatic reset and **MUST** be deactivated
explicitly when exhausted or expired.

> **Notation.** Throughout this document, periods are referenced by their short instance names (`day`, `week`, `month`,
> `year`, `one-time`) in examples and use-case payloads for readability. API requests, storage rows, and
> outbox events use the full GTS URI form shown above (with `one_time` underscore form for the `one-time` instance).

The system **MUST** persist the current period boundary timestamp with each consumption Quota's counter to support
deterministic period-rollover detection.

- **Rationale**: Calendar alignment matches operator and user mental models (billing-cycle alignment is a separate
  concern handled by the billing layer); UTC eliminates timezone-related drift across multi-region deployments.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Period Rollover Semantics

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-period-rollover`

At every period boundary the system **MUST** reset the consumed counter to zero; any unused capacity is forfeited.
Period-rollover transitions **MUST** be atomic with respect to in-flight operations and **MUST** follow these explicit
ordering rules to make period attribution and event semantics deterministic:

- **Boundary attribution (direct debit)**: a single-shot `debit` whose mutation commits at exactly `boundary_at` is
  accounted to period `P+1` (the boundary is inclusive on the new side). Operations whose mutation transaction commits
  at timestamp `t` are accounted to the period whose half-open interval `[start, end)` contains `t`.
- **Boundary attribution (lease)**: a lease is attributed to the period containing its **acquisition** timestamp;
  commit/release/auto-release that fire after a period boundary apply against the acquisition period's counter, not the
  new period's. See `cpt-cf-quota-enforcement-fr-lease-commit` "Cross-period-boundary commit" for the normative rule.
- **`period-rollover` event ordering**: the `period-rollover` event for period `P` is emitted strictly after the last
  commit attributed to `P`.

Period rollovers **MUST** emit a `period-rollover` notification event including the closing-period consumed amount, the
closing-period cap, and the new period boundary. Downstream consumers (Quota Manager) **MAY** derive unused-capacity
figures from this payload for subscription-tier accounting external to QE.

**Event timing — calendar boundary vs event emission.** Because leases attributed to period `P` MAY commit up to
`max_lease_ttl` (default: 1 hour per `cpt-cf-quota-enforcement-fr-lease-acquire`) past `period_end`, the
`period-rollover` event for `P` MAY be delayed by up to `max_lease_ttl` after the calendar boundary. The event therefore
signals **settlement completion** (closing-period `consumed` is final, no further P-attributed mutations will arrive) —
not the calendar transition itself. The calendar boundary is a deterministic function of `time` and `period_spec`;
consumers needing a calendar-time signal compute it directly without observing any event.

Closure rules for credit and rollback (per `cpt-cf-quota-enforcement-fr-credit` and
`cpt-cf-quota-enforcement-fr-rollback`) are intentionally asymmetric on this axis: credit closes at the calendar
boundary (new intent is rejected immediately at `time >= period_end`), while rollback closes at `period-rollover` event
emission (so cross-period lease commits remain reversible during the settlement window).

- **Rationale**: "Use it or lose it" matches the licensing service P1 PRD — neither layer models period-end carry-over
  inside the enforcement layer. Subscription-tier semantics that need to roll unused capacity forward (telco-style
  "rollover minutes", LLM-token credit grants) belong in the licensing layer, which subscribes to `period-rollover` and
  pushes new caps via `update_quota`. Keeping QE rollover atomic and policy-free preserves a small, predictable
  enforcement surface; future re-introduction is tracked in §13 Open Questions.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`

### 5.5 Quota Operations: Debit, Credit, Rollback, Preview

#### Debit Operation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-debit`

The system **MUST** provide an idempotent debit operation. Debit invokes multi-quota evaluation (
`cpt-cf-quota-enforcement-fr-multi-quota-evaluation`) under the active Quota Resolution Engine (
`cpt-cf-quota-enforcement-fr-quota-resolution-engine`); the Engine returns a Decision whose `debit_plan` names exactly
which Quotas to mutate and by how much. The system **MUST** then atomically apply that plan: for every Quota in
`debit_plan`, decrease that Quota's remaining capacity by `entry.amount` (allocation Quotas: increment the in-flight
counter; consumption Quotas: increase the current-period consumed amount). Quotas not named in `debit_plan` **MUST NOT**
be mutated. Debit **MUST** require a client-supplied idempotency key; replay of the same key for the same
authorized attribution, metric, and amount **MUST** return the original Decision without modifying any counter a second
time.

The request **MUST** carry a positive integer `amount > 0`. Requests with `amount ≤ 0` (zero or negative) **MUST** be
rejected with an actionable `INVALID_AMOUNT` error **before** idempotency lookup, subject resolution, or any other
pipeline step — no idempotency record is persisted, no operation log entry is written, no counter is mutated.
Zero-amount debits are rejected because they would consume idempotency / operation-log storage without changing any
counter and would muddy threshold-crossed and replay semantics; callers needing a "would this be allowed?" affordance
without committing capacity **MUST** use `cpt-cf-quota-enforcement-fr-evaluate-preview`.

The Engine returns a `Decision` (shape per §3.4 Decision Contract; binding-Quota selection rules and per-entry
Debit-Plan invariants in §5.9 canonical FR). The system **MUST** apply the returned `debit_plan` atomically — the
caller never observes a state where `result = Allowed` but only some entries were applied, nor where `Denied` or any
canonical error was returned but any counter changed.

- **Rationale**: Atomic decision-and-mutation eliminates the race window where duplicate concurrent calls could each see
  the same remaining capacity and both succeed against a hard cap. Idempotency keys make at-least-once retry semantics
  safe. Driving mutations from the Engine's `debit_plan` (rather than from a hardcoded "debit every applicable Quota
  with the full request amount") lets the default `most-restrictive-wins` Engine implement subject-scope cascade
  single-entry selection while leaving cross-tier splits, proportional distributions, attribute-routing, and
  AND-across-tiers patterns expressible in the Policy layer through `cel` — all without changes to the core
  evaluation engine.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Credit Operation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-credit`

The system **MUST** provide an idempotent S2S credit operation that increases the remaining capacity of **a single
caller-named Quota** by a positive integer amount. The request **MUST** carry an explicit `quota_id` identifying the
target Quota; credit does **not** invoke subject resolution or the Quota Resolution Engine — it mutates the named
Quota's counter directly. The request's explicit target tenant **MUST** be PDP-authorized for the authenticated service
principal and match the named Quota; cross-tenant credit attempts are rejected per `cpt-cf-quota-enforcement-fr-tenant-isolation`. A
request whose `quota_id` is unknown, refers to a deactivated Quota, or refers to a Quota outside the caller's tenant
**MUST** be rejected with an actionable error before any mutation.

The request **MUST** carry a positive integer `amount > 0`. Requests with `amount ≤ 0` (zero or negative) **MUST** be
rejected with an actionable `INVALID_AMOUNT` error **before** idempotency lookup, the row-locked Quota read, or any
other pipeline step — no idempotency record is persisted, no operation log entry is written, no counter is mutated.

For allocation Quotas, credit decrements the in-flight counter (with a floor of zero). For consumption Quotas, credit
decreases the current-period consumed amount (with a floor of zero). Credit **MUST** require a client-supplied
idempotency key; replay of the same key **MUST** return the original outcome without modifying any counter a second
time.

Credit operates against the **currently-active** period of a consumption Quota. Attempts to credit a period whose
calendar window has elapsed (`time >= period_end` at the moment the credit transaction is evaluated) **MUST** be
rejected with an actionable `PERIOD_CLOSED` error in P1. Closure is keyed on **calendar time**, not on `period-rollover`
event emission per `cpt-cf-quota-enforcement-fr-period-rollover` "Event timing" — credit is rejected immediately at the
calendar boundary, even while lease flow may still be settling cross-period commits into the closing period's counter.
Backdated credits to a closed period are deferred (target resolution: production feedback) and would require
operator-elevated authorization. Consumer-side refund-on-cancel for a specific prior debit is served by the **rollback**
primitive (`cpt-cf-quota-enforcement-fr-rollback`), not by credit.

Every successful credit **MUST** emit a `quota-counter-adjusted` notification event carrying the credited amount, the
target `quota_id`, and the authenticated service principal, so tenant-side dashboards remain consistent with
mid-period counter adjustments that are not the result of a natural debit/rollback flow.

- **Rationale**: Credit is the explicit redistribution / compensatory primitive that authorized services, including
  Quota Manager, compose into higher-level workflows (redistribution, SLA-breach grants, manual adjustments). These workflows
  always target a specific Quota — "give user U 500 more tokens", "restore the EU-region cap by 100 hours". Rollback is
  the consumer-side undo for a specific prior debit and is identified by the original idempotency key;\
  credit and rollback are intentionally separated by *identity of the operation* (free-form adjustment vs. undo of a
  specific debit). PDP authorization of the explicit target prevents unauthorized adjustments. Rejecting credits to
  closed periods prevents retroactive accounting that would invalidate already-emitted
  `period-rollover` events. The `quota-counter-adjusted` event keeps observability on tenant-facing surfaces consistent
  when counters move outside the debit/rollback flow.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`, `cpt-cf-quota-enforcement-actor-quota-manager`

#### Rollback Operation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-rollback`

The system **MUST** provide an idempotent rollback operation that reverses a previously committed debit, identified by
the original idempotency key. Rollback restores the counter to the state it would have been in had the debit never
occurred. Replay of the same rollback **MUST** be a no-op after the first invocation. A rollback request whose
original-debit key is not found **MUST** be rejected with an actionable `UNKNOWN_OPERATION` error.

**Rollback against lease-derived debits.** Rollback **MAY** target any committed debit, including debits that originated
from a lease `commit` (per `cpt-cf-quota-enforcement-fr-lease-commit`) — the commit operation produces a debit record
under the **commit call's** idempotency key, and rollback identifies it via that same key. This is symmetric with
single-shot debits: in both cases rollback's identity is "undo the debit registered under this idempotency key",
regardless of whether the debit arose from a direct `debit` call or a lease `commit`. The closed-period guard above
applies uniformly: a rollback against a lease-commit debit whose attribution period has closed is rejected with
`PERIOD_CLOSED`. (Rollback of a lease **before** commit is not the rollback primitive's job — use `release` per
`cpt-cf-quota-enforcement-fr-lease-release`.)

**Rollback against a settled period (consumption Quotas).** A rollback request targeting a debit whose attribution
period has been **fully settled** (the `period-rollover` event has been emitted for it per
`cpt-cf-quota-enforcement-fr-period-rollover`) **MUST** be rejected with an actionable `PERIOD_CLOSED` error in P1.
Rollback closure is keyed on **settlement** (event emission), not on the calendar boundary — this is intentionally
asymmetric with `cpt-cf-quota-enforcement-fr-credit`'s calendar-keyed closure. The settlement window — between
`period_end` and `period-rollover` event emission — is exactly when lease cross-period commits land against the closing
period's counter; rollback remains possible during this window so cross-period commits remain reversible. Once
`period-rollover` fires, the closing-period `consumed` is final, and any rollback to that period would invalidate
already-emitted payloads (and any downstream usage telemetry derived from them). Callers needing guaranteed cancellation
across a period boundary **MUST** model the work via the lease flow (`cpt-cf-quota-enforcement-fr-lease-acquire` /
`commit` / `release`) so that period attribution stays bound to acquisition time per
`cpt-cf-quota-enforcement-fr-lease-commit` "Cross-period-boundary commit". Backdated rollbacks against settled periods
are deferred (target resolution: production feedback) and would require operator-elevated authorization together with
the audit infrastructure tracked in §13.

Rollback semantics are equivalent to refund-on-cancel for the canceling caller's purposes — a job that aborted before
consuming its reserved or debited capacity can rollback to release it. Rollback against a credit operation is **NOT**
supported (credits are themselves corrective and intentionally not reversible via rollback; use a counter-credit if
needed).

Every successful rollback **MUST** emit a `quota-rollback-applied` notification event (catalogue kind in
`cpt-cf-quota-enforcement-fr-notification-plugin`) carrying the original debit's idempotency key, the rolled-back
amount, the target Quota, and the consumer identity from the SecurityContext, so subscribers observe rollback effects
distinctly from credits (which use `quota-counter-adjusted`) and from natural debit completion (which is silent on the
notification surface). Rollbacks against a settled period are rejected before any mutation per the rule above and emit
no event.

- **Rationale**: Rollback by original-key gives callers a deterministic compensation primitive without requiring them to
  remember the debit amount or recompute the inverse — the original idempotency key is sufficient. It is intentionally
  separate from credit because rollback's identity is "undo this specific debit", whereas credit's identity is "issue
  this specific refund".
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Evaluate Preview (read-only dry-run)

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-evaluate-preview`

The system **MUST** provide a read-only **preview** operation that executes the full evaluation pipeline (subject
resolution → applicable-Quotas → active Quota Resolution Policy → Engine → Decision) for a
same attribution, metric, amount, metadata, and optional resource input shape and returns the resulting Decision **without applying any counter
mutation, without holding any capacity, and without persisting any idempotency record or operation-log entry**. Preview
is intended for:

- Quota Manager UIs that surface "if you debit X right now, here is what would happen" affordances to tenant
  administrators and end users.
- Operator authoring and testing of `cel` (and future) Engine Policies — exercising candidate Policy logic against real
  Quota state before deploying.
- Calling-service capacity planning and debugging ("would my next batch fit?", "why was the last call denied?").

Preview **MUST**:

- Require the same PDP authorization (`cpt-cf-quota-enforcement-fr-authorization`) as the corresponding write operation;
  reads outside the caller's authorized metric / subject scope are rejected.
- Apply the same trust-boundary rules (§3.4) — Decision-shaped fields are silently ignored if present in the request
  body.
- Use the **current** counter state at evaluation time (a transactional snapshot read; no read-modify hold and no
  contention against concurrent debits).
- Return the full Decision shape (`result`, `debit_plan`, `diagnostics`) the system would produce for an equivalent
  debit, plus an explicit `preview: true` marker so callers cannot conflate it with a real Decision. Diagnostics include
  `policy_id` and `policy_version` (per `cpt-cf-quota-enforcement-fr-quota-resolution-policy-versioning`) so operators
  inspecting "what would happen if" see exactly which Policy version they are testing against.
- **NOT** require an idempotency key — preview is read-only and idempotent by construction.
- **NOT** persist any record — no operation log entry, no idempotency record, no lease row, no Engine-evaluation record
  beyond ephemeral telemetry counters.

Preview **MUST NOT** be used as a substitute for `reserve` when capacity needs to be held — preview's verdict can be
invalidated by concurrent debits between the preview call and a follow-up real debit. Callers needing held capacity
continue to use `reserve` (`cpt-cf-quota-enforcement-fr-lease-acquire`).

- **Rationale**: A read-only preview is a more-ergonomic alternative to `reserve(ttl=1s) + release(token)` for the
  common "what would happen if I debit X?" question — no idempotency-key generation, no capacity hold, no two-phase
  commit, no row mutation, no sweeper churn, no contention with concurrent writers. The `reserve+release` pattern
  remains the right tool when the caller actually wants to hold capacity briefly; preview is for the cases where the
  caller never intended to consume yet (UI affordances, Policy authoring, capacity planning). PDP scoping
  (`cpt-cf-quota-enforcement-fr-authorization`) and tenant-isolation are the only access controls; preview shares the
  standard hot-path NFR with debit and is bounded by the same baseline throughput protections that apply to every read
  operation.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`, `cpt-cf-quota-enforcement-actor-quota-manager`,
  `cpt-cf-quota-enforcement-actor-quota-reader`

### 5.6 Lease & Two-Phase Operations

#### Lease Acquisition

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-lease-acquire`

The system **MUST** provide a lease operation that holds capacity against every applicable Quota for a bounded TTL
without finalizing consumption. A successful lease **MUST** return an opaque lease token and **MUST** decrease each
applicable Quota's remaining capacity by the reserved amount for the TTL duration. Lease requires an idempotency key;
replay returns the original outcome (the token, or the original `Denied` Decision). Lease **MUST** be subject to multi-quota evaluation identically to debit
(`cpt-cf-quota-enforcement-fr-multi-quota-evaluation`); a lease that would exceed any applicable Quota **MUST** be
denied without holding capacity in any Quota.

The request **MUST** carry a positive integer `amount > 0`. Requests with `amount ≤ 0` (zero or negative) **MUST** be
rejected with an actionable `INVALID_AMOUNT` error **before** idempotency lookup, the multi-quota evaluation, or any
hold acquisition — no idempotency record is persisted, no lease row is created, no Quota's capacity is reserved, the
per-`(tenant, metric)` active-lease counter is unchanged.

The lease TTL **MUST** be supplied by the caller and **MUST** fall within an operator-configurable window
`[min_lease_ttl, max_lease_ttl]` (platform defaults: 1 s and 1 h respectively). A TTL outside the window — or a missing
`ttl` field — **MUST** be rejected with an actionable `TTL_OUT_OF_BOUNDS` error **before** idempotency lookup,
multi-quota evaluation, or any hold acquisition — no idempotency record, no lease row, no capacity hold, no change to
the per-`(tenant, metric)` active-lease counter (mirroring the `INVALID_AMOUNT` fail-fast above). Clamping is **NOT**
performed: silent TTL adjustment would mask client miscalculations and is incompatible with the lease contract, which
entitles the holder to the exact TTL it reserved.

The system **MUST** persist the lease token, the held amount, the affected Quotas, the owning subject context, and the
expiry timestamp.

**Atomic multi-Quota acquisition.** When the Engine's Debit Plan names multiple Quotas (e.g., a user-scoped Quota plus a
tenant-scoped Quota that both apply to one operation, or several Quotas selected by a cascade or attribute-based
Policy), the system **MUST** acquire holds on every Quota in the plan **atomically**: either every named Quota's
capacity is held or none is. Partial holds (some Quotas reserved, others not) are not a permitted intermediate state,
including under failure or concurrent contention. Concurrent multi-Quota lease traffic **MUST NOT** deadlock the
acquisition path. The choice of acquisition ordering, locking discipline, and storage-plugin transactional primitives
that achieve these properties is a DESIGN-time concern and is described in DESIGN §3.3 (storage-plugin invariants
I7/I8/I9) and §3.6 sequence diagrams.

**Acquisition contention timeout.** Concurrent leases and debits against the same Quota row contend for the underlying
counter mutation. The system **MUST** bound how long an acquisition request waits on contention via an
operator-configurable per-metric **acquisition contention timeout** (default: 0 ms — fail-fast). When the timeout is
exceeded, the acquisition **MUST** be rejected with an actionable `LEASE_CONTENTION_TIMEOUT` error and **MUST NOT** hold
any Quota (consistent with the atomic-acquisition rule above). The scope is per-metric (not per-tenant) because
row-level contention is a platform-tuning concern driven by metric hotness — different metrics ( `ai-tokens-input` vs
`vCPU-hours`) have different contention profiles, but a single metric's contention curve does not vary meaningfully by
tenant. This timeout is distinct from:

- the **lease TTL** (1 s – 1 h, controls how long a lease is held *after* successful acquisition),
- the **per-Policy Engine evaluation timeout** (default 5 ms, the Engine compute budget per
  `cpt-cf-quota-enforcement-fr-quota-resolution-policy`),
- the **per-`(tenant, metric)` active-lease cap** (default 1000, bounds concurrent live leases per
  `cpt-cf-quota-enforcement-fr-lease-timeout`).

Operators MAY tune all four independently. The same acquisition contention discipline conceptually applies to debit,
credit, rollback, commit, release, and batch_debit (every counter-mutating operation experiences the same row
contention); the normative timeout and telemetry are stated here once.

**Contention telemetry.** The system **MUST** expose:

- a counter `lease_contention_rejected_total` incremented on every `LEASE_CONTENTION_TIMEOUT` rejection, labelled by
  canonical registered `metric`,
- a histogram `lease_acquisition_wait_seconds` covering the wait time before successful acquisition or rejection,
  labelled by canonical registered `metric`.

These counters belong to the §5.16 telemetry surface. They let operators detect hot-key contention (single-row write
hotspot per the §12 risk) before it manifests as user-visible latency, and they let Quota Manager surface
contention-driven denials distinctly from `LEASE_INFLIGHT_LIMIT_EXCEEDED`, `LEASE_NOT_ACTIVE`, and Engine-`Denied`
rejections in operator UIs.

- **Rationale**: Two-phase lease models long-running operations whose consumption isn't known until completion ( e.g.,
  "I'm starting a 30-minute job that may abort"). Without lease, callers must invent ad-hoc pending-debit tables or risk
  over-committing capacity. Atomic multi-Quota acquisition is the only safe semantic when one operation needs holds
  across several Quotas — partial holds would produce ambiguous state for callers and dirty rows for the sweeper. The
  acquisition contention timeout (default 0 ms / fail-fast) makes the system's behavior under hot-key contention
  deterministic and observable rather than letting waiting requests pile up implicitly behind row locks; operators tune
  the timeout based on workload preferences (low latency vs higher acquisition success rate).
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Lease Commit

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-lease-commit`

The system **MUST** provide a commit operation that converts an active lease into a debit. Commit **MUST** be idempotent
(replay returns the original outcome). Commit **MAY** specify an `actual_amount` less than or equal to the originally
reserved amount; the difference (reserved minus actual) is returned to each affected Quota's remaining capacity.
`actual_amount > reserved_amount` **MUST** be rejected with an actionable `OVER_COMMIT_NOT_AUTHORIZED` error — callers
needing to commit beyond their lease must reserve a higher worst-case estimate up front. Committing an expired or
already-resolved (committed/released) lease **MUST** be rejected with an actionable `LEASE_NOT_ACTIVE` error.

**Cross-validity-boundary commit.** A commit MAY arrive after the underlying Quota's `validity_window` has expired (the
lease was acquired while the window was still valid; commit happens after `validity_end`). Such commits **MUST** succeed
and the corresponding counter mutation **MUST** be applied: the lease guarantee was earned at acquisition time and the
holder is entitled to its capacity. Operators wanting a strict cutoff at `validity_end` should constrain TTL bounds (per
`cpt-cf-quota-enforcement-fr-lease-acquire`) so that no lease can outlive the validity window. Commits whose underlying
lease TTL has expired remain rejected per the rule above (lazy expiry of TTL is independent of validity-window expiry).

**Cross-period-boundary commit (consumption Quotas).** A lease is attributed to the consumption-Quota period whose
half-open interval `[period_start, period_end)` contains its **acquisition timestamp**. The held amount counts against
that period's `consumed` from the moment of acquisition until the lease reaches a terminal state (`committed` /
`released` / auto-released); it does **NOT** count against any subsequent period's capacity, even if the lease's TTL
crosses one or more period boundaries. A `commit` that fires after a period rollover **MUST** apply its counter mutation
against the **acquisition period's** counter; the new period's `consumed` is not incremented by such a commit.
Symmetrically, a `release` (or TTL auto-release) after a period boundary returns held capacity against the acquisition
period's counter only — the new period's `remaining` is unaffected. This mirrors the cross-validity-boundary rule above:
the lease guarantee, including its period attribution, is earned at acquisition time. Operators wanting strict
period-boundary cutoffs should constrain lease TTL so that leases cannot outlive the acquisition period.

- **Rationale**: Allowing the actual committed amount to be less than reserved is the common case (the job consumed less
  than budgeted); rejecting `actual > reserved` unconditionally prevents silent over-commit and keeps the lease contract
  tight — the held capacity is the upper bound the caller is entitled to consume.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Lease Release

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-lease-release`

The system **MUST** provide a release operation that returns a lease's held amount to every affected Quota's remaining
capacity without committing a debit. Release **MUST** be idempotent (replay is a no-op after the first invocation).
Releasing an already-resolved lease **MUST** be rejected with an actionable `LEASE_NOT_ACTIVE` error.

- **Rationale**: Release is the inverse of lease; together with TTL-based auto-release it ensures held capacity is
  always eventually returned to the affected Quotas even if the calling service crashes or is partitioned.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Lease TTL Auto-Release

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-lease-timeout`

The system **MUST** automatically release every lease whose TTL has expired without a commit or release. Auto-release
**MUST** be deterministic with respect to the lease expiry timestamp. The requirement is fulfilled by **two distinct
tiers**, deliberately separated so that correctness never depends on a background process being live:

1. **Semantic release (lazy, correctness-critical).** Once a lease's expiry timestamp has passed, the held amount **MUST
   NOT** be counted against any affected Quota's in-flight capacity. Every reader and every writer **MUST** treat the
   lease as released, regardless of whether its underlying storage row still exists or has been reclaimed. Semantic
   release is the only tier on which correctness depends and **MUST** remain correct under sweeper outage, partition,
   restart, or any other lifecycle event of the reclamation tier below. New leases and debits **MUST NOT** be blocked by
   zombie rows of already-expired leases.

1. **Physical reclamation (eventual, storage hygiene).** A background sweeper **MUST** be deployed to perform physical
   cleanup of expired lease rows — deletion, or operator-defined archival to a long-term store. Reclamation **MUST**
   complete within an operator-configurable interval after expiry (default: 1 hour). The sweeper is also the canonical
   emission point for the `lease-auto-released` notification event: exactly one event per lease, carrying the lease ID,
   owning subject context, held amount, affected Quotas, and expiry timestamp.

Sweeper liveness **MUST NOT** gate correctness. If the sweeper is delayed, paused, or crashed, the semantic-release tier
continues to behave correctly — expired leases remain released for accounting purposes, and new operations are not
blocked. Unreclaimed rows accumulate until the sweeper resumes, and the corresponding `lease-auto-released` events are
deferred until reclamation; the system **MUST** surface unreclaimed-expired-lease count via telemetry so operators can
detect sweeper outages.

**Per-`(tenant, metric)` active-lease cap.** To bound the unreclaimed-row ceiling under sweeper failure or under abusive
usage patterns (callers that open leases and never settle them), the system **MUST** enforce an operator-configurable
per-`(tenant, metric)` cap on concurrent **active** leases (default: 1000). Lease acquisition (
`cpt-cf-quota-enforcement-fr-lease-acquire`) **MUST** reject with an actionable `LEASE_INFLIGHT_LIMIT_EXCEEDED` error
any request that would push the active-lease count above the cap, regardless of underlying Quota capacity. Expired
leases do **not** count toward the cap (they are released by the lazy semantic above) — the cap exists to limit live
in-flight leases and to bound row growth between sweeper runs.

- **Rationale**: Lease safety depends on the absence of zombie holds. Lazy expiry interpretation makes the semantics
  safe under partitions or sweeper outages — an expired lease never blocks new operations even if the ledger row hasn't
  been physically reclaimed yet. Bounding reclamation lag and capping concurrent active leases together prevent
  unbounded storage growth from misbehaving callers and from sweeper outages, without coupling correctness to sweeper
  liveness.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

### 5.7 Bulk Operations

#### Batch Debit

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-batch-debit`

The system **MUST** provide a batch debit operation that submits multiple debit items targeting potentially different
metrics in a single call. The batch **MUST** carry an envelope idempotency key; replay of the same envelope key returns
the original outcome without modifying counters a second time. Each item **MUST** also carry its own idempotency key for
individual identification.

Every batch item **MUST** carry a positive integer `amount > 0`. The batch **MUST** be rejected with an actionable
`INVALID_AMOUNT` error (envelope-level, naming the offending item index) **before** any item is evaluated, regardless of
`mode`, if **any** item carries `amount ≤ 0`. No envelope idempotency record is persisted, no per-item idempotency
records are persisted, no counter is mutated.

The system **MUST** support two batch modes selected by the caller via a required `mode` field on the request:

1. **`atomic` (P1).** All-or-nothing semantics for **multi-metric admission of a single logical operation**. Every item
   is evaluated against its own applicable-Quotas set under the active Quota Resolution Engine; each item's evaluation
   **MUST** observe counter state reflecting the application of every previously-evaluated item in the same batch,
   otherwise the Engine cannot produce a correct Debit Plan. If **every** item evaluates to `Allowed`, the system
   **MUST** apply the union of all per-item `debit_plan`s; if **any** item evaluates to `Denied` or fails with a
   canonical error, the system **MUST** apply no counter mutations and report the batch outcome as `Denied` (in the
   body) or surface the canonical error accordingly, with per-item statuses included for diagnostic purposes only.
   Atomic mode is the natural primitive for "this single operation needs metric A AND metric B AND metric C — admit or
   deny as a whole" (e.g., an LLM call that consumes input tokens, output tokens, and compute seconds simultaneously).

1. **`independent` (P2).** Partial-success semantics for **bulk independent operations** submitted in one RPC purely as
   a transport optimization (e.g., recording consumption for N unrelated background jobs). Each item is evaluated and
   applied independently; per-item outcomes do not affect each other. **P1 implementations MUST reject
   `mode = independent` requests with an actionable `NOT_YET_IMPLEMENTED` error** until the mode ships.

The response **MUST** include the batch-level outcome and a per-item array preserving submission order. Callers **MUST**
treat `Denied` and a canonical error distinctly: `Denied` is a deterministic over-cap signal in the Decision body (retry
is futile until a credit or period rollover), whereas a canonical error is a non-deterministic technical failure (retry
under the same envelope key is replay-safe).

The system **MUST** enforce a configurable maximum batch size (default: 100 items per batch) and reject oversized
batches with an actionable error.

**Batch-level evaluation timeout.** Per-Policy Engine timeouts (default 5 ms per
`cpt-cf-quota-enforcement-fr-quota-resolution-policy`) compose poorly with batches: a 100-item batch could spend up to
`100 × 5 ms = 500 ms` on Engine evaluation alone. The system **MUST** therefore enforce a **batch-level evaluation
timeout** that supersedes per-Policy timeouts for the batch as a whole. The timeout is a single
**operator-configurable** flat duration applied to the entire batch evaluation; deployment-default **250 ms**.
Worst-case batch latency is bounded by this timeout together with the maximum batch size (default 100 items per batch);
adaptation to load is the caller's responsibility (client-side retry / batch splitting), not a server-side concern.

A batch-level timeout fire in **atomic** mode **MUST** surface a canonical `DeadlineExceeded` error (with
`reason = "BATCH_TIMEOUT"` carried in the envelope) for the whole batch with no counter mutations applied; the caller
retries with the same envelope key (replay-safe) or, if persistent, with a smaller batch under a new envelope key. In
**independent** mode (P2), items whose Engine evaluation has not completed when the batch-level timeout fires **MUST**
be reported in the per-item array as a canonical `DeadlineExceeded` failure (`reason = "BATCH_TIMEOUT"`) with no counter
mutations for those items; items already evaluated within the budget retain their `Decision` outcomes, and items pending
in the queue at fire-time are also reported with `BATCH_TIMEOUT`.

- **Rationale**: Single-item idempotency keys do not compose naturally for multi-metric debits common in cross-resource
  operations. Atomic mode preserves the user-facing all-or-nothing semantics of a single logical operation that consumes
  multiple resources — without atomicity, a partial application leaves counters inconsistent with the operation the
  caller actually performed (e.g., input tokens deducted but the call ultimately denied on compute), forcing brittle
  compensating logic at every consumer. Independent mode is deferred to P2 because shipping it alongside atomic from day
  one risks callers picking the wrong mode for multi-metric admission and inheriting that footgun; the bulk-independent
  use case can be served in P1 by issuing N parallel single-item debits. The batch-level timeout prevents per-Policy
  timeouts from accumulating linearly into NFR-violating worst-case latency.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

### 5.8 Idempotency

#### Idempotency Guarantee for All Write Operations

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-idempotency`

The system **MUST** require a client-supplied idempotency key on every write operation: debit, credit, rollback,
reserve, commit, release, and every batch envelope. Keys are scoped per
`(tenant_id, idempotency_subject_key, operation_type, key)`. For consumer operations, `tenant_id` and the attribution
used to calculate `idempotency_subject_key` are caller-supplied but become trusted only after PDP authorization
(`cpt-cf-quota-enforcement-fr-authorization`). Different tenants, subject keys, or operation types using the same key
string create **independent idempotency records** — they are never cross-matched.

`idempotency_subject_key` is a typed, fixed-width fingerprint of a canonical subject set. Its source is deterministic
per operation kind:

- For `debit` and `reserve`, the system **MUST** fingerprint the complete authorized attribution set after mapping it
  through the catalogue, sorted and deduplicated by `(projection_type, subject_id)`. For a `batch_debit` envelope it
  fingerprints the sorted, deduplicated union of all item sets. Callers cannot select a concrete projection.
- `commit` and `release` **MUST** reuse the subject key persisted by the corresponding lease acquisition; they do not
  re-resolve it against the current catalogue.
- For operations that target a Quota by explicit `quota_id` and do not invoke subject resolution — `credit`, `rollback`
  — the system **MUST** fingerprint the owning Quota's persisted `(projection_type, subject_id)` pair, read server-side
  under the same row lock that performs the mutation. The key **MUST NOT** use `quota_id` or an unvalidated subject.

These rules keep the four-component scope `(tenant_id, idempotency_subject_key, operation_type, key)` total across every
write operation and prevent caller-selected projections from fragmenting an applicable user or tenant scope. P1 does
not support breaking projection-catalogue activation; lease follow-up operations remain
stable because they reuse the acquisition key (`cpt-cf-quota-enforcement-nfr-idempotency-guarantee`).

Any future breaking projection-catalogue activation **MUST** preserve the previous mapping from logical attribution to
`idempotency_subject_key` for every unexpired idempotency record until that record's configured retention window expires.
A retry within the window **MUST** compute the original key and return the stored outcome; catalogue activation
**MUST NOT** move it into a new idempotency scope or cause a second counter effect.

Within a single `(tenant, idempotency_subject_key, operation_type, key)` scope, a submitted key falls into one of two
cases:

1. **Exact replay** — same scope and identical payload as the original request. The system **MUST** return the original
   outcome (Decision — including `result`, `debit_plan`, `diagnostics`, and the originally returned identifiers and
   amounts) without re-evaluating, without modifying counters, and without re-binding the non-deterministic
   EvaluationContext fields (notably `time`). A replay run at a different wall-clock time **MUST** still produce the
   original verdict; this is required for time-gated Policies (e.g., an Engine config that blocks during a maintenance
   window) to remain replay-safe. The persisted idempotency record **MUST** capture the full Decision blob plus the
   `engine_id`, `policy_id`, and `policy_version` under which the original Decision was produced; replay diagnostics
   surface those attribution fields verbatim for audit and forensics.

1. **Payload divergence** — same scope but a payload that differs from the original (different amount, different metric,
   etc.). The system **MUST** reject with an actionable `IDEMPOTENCY_PAYLOAD_MISMATCH` error and **MUST NOT** touch the
   original record.

Replay requests **MUST NOT** carry any Decision-shaped field in the body; per §3.4, the server silently ignores any such
fields if present, regardless of which branch above would otherwise apply.

The system **MUST** preserve the idempotency guarantee for a configurable retention window (default: 24 hours)
sufficient to bound legitimate retry windows. The retention window is operator-configurable per `(tenant, metric)` —
matching the scope of the idempotency key itself (`(tenant_id, idempotency_subject_key, operation_type, key)` per the scoping rule
above; metric is the natural axis along which retry-window practice differs across consuming services). Replays
attempted after the window has expired for a given key **MUST** be treated as new operations (re-evaluated against
current state) — this is intended behavior since legitimate retries fall well within the window.

- **Rationale**: At-least-once delivery from upstream callers (network retries, message-queue redelivery, background-job
  duplicate execution) is a fact of life; without strict idempotency the counter integrity collapses under load.
  Payload-mismatch detection prevents accidental key reuse across distinct logical operations.
- **Key-generation guidance** (non-normative): recommended key generators are UUIDv4 (collision-safe by construction) or
  structured `<consumer-prefix>-<request-id>` patterns where `<request-id>` is itself collision-free per the consumer's
  own discipline (e.g., a UUIDv4 minted at request-receipt time). Anti-patterns include hashes of mutable inputs (
  request URL, request body) — two distinct operations that happen to share the hashed inputs would silently collide.
  The system does **NOT** validate idempotency-key generator quality and **MUST NOT** infer collision behavior from the
  key shape — collisions across distinct logical operations land as `IDEMPOTENCY_PAYLOAD_MISMATCH` errors at the
  consumer's expense. Consumers (including Quota Manager and direct Quota Consumers) own this discipline.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

### 5.9 Multi-Quota Evaluation, Quota Resolution Policy & Pluggable Engine

#### Multi-Quota Evaluation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-multi-quota-evaluation`

The system **MUST** evaluate every operation against the full set of applicable Quotas produced by subject resolution (
`cpt-cf-quota-enforcement-fr-subject-resolution`). Evaluation **MUST** consider every active Quota whose
`(projection_type, subject_id)` is in the applicable-subjects set and whose metric matches the operation's metric.
Evaluation **MUST** select the active Quota Resolution Policy (`cpt-cf-quota-enforcement-fr-quota-resolution-policy`)
and invoke the Engine that the Policy references (`cpt-cf-quota-enforcement-fr-quota-resolution-engine`) to produce a
Decision (§3.4).

Evaluation results **MUST** include, for each applicable Quota, the quota ID, type (allocation/consumption),
`enforcement_mode`, current consumed/in-flight amount, cap, and the contribution to the final Decision (e.g., "this
Quota was selected by the Debit Plan with `amount=50`"). This per-Quota detail is included in the Decision response so
callers can surface useful diagnostics.

- **Rationale**: Without explicit multi-quota evaluation, callers must perform N round trips to Quota Enforcement for N
  applicable Quotas, breaking the latency budget. Single-call evaluation with full per-Quota detail also provides the
  information needed for downstream products to render meaningful UI explanations.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Quota Resolution Policy

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-resolution-policy`

A **Quota Resolution Policy** is a separate operator-managed entity that selects which Quota Resolution Engine to use
for a given scope and supplies that Engine's config. The system **MUST** model the Policy as a first-class entity with:

- a unique stable policy ID,
- a scope (P1, closed enum): `global` or `metric=<metric_name>`. The scope representation is intentionally extensible —
  narrower scopes (e.g., per-subject) are not specified in P1 and are tracked in §13 Open Questions; the persistence and
  selection layers should be designed to admit additional scope levels without breaking changes, but no narrower scope
  is reachable through the P1 API,
- an `engine_id` referencing a registered Engine (P1: `most-restrictive-wins` or `cel`; future: any Engine registered
  per `cpt-cf-quota-enforcement-fr-quota-resolution-engine`),
- an opaque `engine_config` (engine-specific structure; e.g., for `cel` it carries the CEL expression; for
  `most-restrictive-wins` it is empty),
- an optional per-Policy evaluation timeout (operator-configurable; default 5ms; clamped to operator-configured upper
  bound),
- an optional description.

For each operation, the system **MUST** select the active Policy via most-specific-scope precedence: the `metric` Policy
if one is defined for the operation's metric, else the `global` Policy. The system **MUST** seed a built-in `global`
Policy at gear bootstrap with `engine_id=most-restrictive-wins` and empty `engine_config`; this seeded Policy **MUST
NOT** be deletable but **MAY** be replaced by an operator with a different `global`-scoped Policy referencing any
registered Engine.

Policy creation and update **MUST** delegate `engine_config` validation to the named Engine's config validator (e.g.,
the `cel` Engine performs CEL parse + type-check at this stage; the `most-restrictive-wins` Engine rejects any non-empty
config). Validation failures **MUST** be returned as actionable errors before persistence; persisted Policies are
guaranteed to carry a Engine-validated config.

The validator **MUST** receive the snapshotted request and constraint schemas. The `cel` validator
**MUST** statically check property/projection references and pair compatibility — including type disagreement,
non-intersecting declared domains, and scalar/collection operator mismatch — and return line/column diagnostics.
Contract resolution and snapshotting occur at Policy create/update, never on the evaluation hot path.

Operators **MAY** create, update, and delete narrow-scope Policies; the global Policy **MUST NOT** be deleted. A Policy
referencing an `engine_id` that is not registered in the current gear deployment **MUST** be rejected at create/update
time.

**Versioning.** Every Quota Resolution Policy is versioned per
`cpt-cf-quota-enforcement-fr-quota-resolution-policy-versioning`. Updates create a new immutable version; old versions
remain queryable; rollback is a first-class operation. Evaluation always selects the active version per `policy_id`;
Decision diagnostics carry the `policy_version` evaluated under, so audit and replay attribute every Decision to a
specific Policy state. The seeded built-in global Policy materializes at bootstrap as `policy_version = 1` and follows
the same versioning rules thereafter.

**`engine_config` content discipline — no secrets, no sensitive data.** A Policy's `engine_config` carries arbitration
\* *logic*\*, not sensitive **data**: CEL expressions (and future Starlark / Lua / Wasm engine configs) **MUST NOT**
embed credentials, API keys, tokens, signing keys, pricing or commercial rates, customer identifiers, or any other
content the operator considers sensitive. Engine config is classified as Platform Operational Data per §6.2 alongside
Quota state — it is visible to platform operators with Policy-management PDP grants, surfaced in operation logs and
audit trails (when those land in P2), and not encrypted at rest by the P1 storage plugin. Sensitive values that the
Policy needs to consult **MUST** be referenced indirectly through the validated `request` object (set by the calling service from a
sanctioned config source) or through Quota Metadata keys that the operator populates from a sanctioned config source —
never inlined into the engine_config payload itself. Operators are responsible for upholding this discipline; the system
does not inspect engine_config content for secrets.

- **Rationale**: Separating the Policy entity from the Engine cleanly maps two independent operator concerns: *where* (
  which scope) and *how* (which Engine + config). The P1 scope ladder is deliberately minimal — only `global` and
  `metric` — because no concrete operator use case in P1 requires narrower targeting, and a wider ladder introduces
  selection ambiguities (e.g., multiple projection scopes simultaneously present in the applicable-subjects set produce a
  tie-break problem with no obvious right answer). Keeping the scope representation extensible without prescribing
  additional levels lets P2 (or later) add narrower scopes once production demand surfaces and the right shape is clear.
  Delegating config validation to the Engine keeps the Policy entity engine-agnostic and avoids hardcoding parser logic
  for every supported language in the Policy layer. A per-Policy timeout (rather than a per-Engine global) lets
  operators cap risky customizable-Engine evaluations independently of the fast `most-restrictive-wins` path.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Quota Resolution Policy Versioning

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-resolution-policy-versioning`

The system **MUST** treat every Quota Resolution Policy record as **immutable per version**. Updates **MUST NOT** mutate
existing fields in place; instead, every change produces a new version with a higher `policy_version` integer, and a
separate latest-pointer per `policy_id` names the currently-active version. Rollback to a previous version is a
first-class operation.

**Version record shape**. Each Policy version carries:

- `policy_id` — stable identifier shared across all versions of the same Policy.
- `policy_version: uint32` — monotonically increasing per `policy_id`; first version is `1`.
- `created_at: RFC3339_UTC_ms` — emission timestamp of this version.
- `created_by: string` — caller identity from SecurityContext at version creation (operator / Quota Manager). Captured
  for audit attribution.
- `comment: string` (optional) — operator-supplied free-text note (change rationale, ticket reference, author, or any
  other annotation).
- `version_state: enum { active, superseded, rolled_back, deleted }`:
  - `active` — at most one version per `policy_id` carries this state at any time; the version pointed to by latest.
  - `superseded` — replaced by a later `active` version through normal update flow.
  - `rolled_back` — was active but has been abandoned via rollback (replaced by an earlier version becoming active
    again). Terminal — never re-activated.
  - `deleted` — set on the previously-active version when the entire `policy_id` is soft-deleted via `delete_policy`.
    Terminal — never re-activated. After deletion the `policy_id` has no active version; evaluation falls through to the
    next-most-specific scope per the precedence ladder in `cpt-cf-quota-enforcement-fr-quota-resolution-policy`.

**Bootstrap.** The seeded built-in `global` Policy (per `cpt-cf-quota-enforcement-fr-quota-resolution-policy`)
materializes at gear bootstrap as
`policy_id = global, policy_version = 1, version_state = active, engine_id = most-restrictive-wins, engine_config = {}`.
Operator updates produce versions 2, 3, …. When narrower-scope Policies are deleted or rolled back, the seeded global
remains the ultimate fallback — evaluation never enters a "no Policy applies" state.

**Latest-pointer.** Per `policy_id`, the system **MUST** maintain a separate latest-pointer that names the
currently-active `policy_version`. The latest-pointer is updated atomically with version creation or rollback. Readers
default to the latest version unless they explicitly request a specific version.

**Operations** (version-based optimistic concurrency, no idempotency-key requirement):

- `create_policy(scope, engine_id, engine_config, [comment])` — creates `policy_version = 1` with
  `version_state = active`; latest-pointer initialized. Rejected if an active Policy already exists at the exact same
  scope.
- `update_policy(policy_id, if_match_version, engine_id?, engine_config?, [comment])` — creates `policy_version = N+1`
  with `version_state = active`; previous active version transitions to `superseded`; latest-pointer moved to `N+1`.
  **Rejected with `VERSION_CONFLICT` if `if_match_version` does not equal the current latest** (lost-update protection).
  Atomic.
- `rollback_policy(policy_id, target_version, [comment])` — makes `target_version` active again (its `version_state`
  returns to `active`); the previously-active version transitions to `rolled_back` (terminal). Latest-pointer moved to
  `target_version`. Atomic. Rejected with `UNKNOWN_POLICY_VERSION` if `target_version` does not exist; rejected with
  `VERSION_ROLLED_BACK` if `target_version` is in `rolled_back` state. Naturally idempotent on retry against the same
  target.
- `delete_policy(policy_id, [comment])` — soft-deletes a narrow-scope Policy entirely. The currently-active version
  transitions to `deleted` (terminal); the latest-pointer is cleared so subsequent evaluations falling within this
  Policy's scope fall through to the next-most-specific scope per `cpt-cf-quota-enforcement-fr-quota-resolution-policy`.
  Historical versions retain their existing `superseded` / `rolled_back` state — only the active version moves to
  `deleted`. The seeded global Policy **MUST NOT** be deletable; `delete_policy` against it is rejected with
  `CANNOT_DELETE_SEEDED_GLOBAL_POLICY`. A subsequent `delete_policy` against an already-deleted `policy_id` is a no-op
  (idempotent). A `policy-changed` notification event is emitted with `change_kind = "deleted"` per
  `cpt-cf-quota-enforcement-fr-notification-plugin`. All retained versions (`superseded`, `rolled_back`, and the
  `deleted` terminal version) follow the standard 90-day retention window per the **Retention** paragraph below; after
  the window they are hard-deleted by the storage retention sweeper.
- `read_policy(policy_id, version=N | latest)` — returns the specified or current version. After
  `delete_policy(policy_id)`, `latest` returns the `deleted` terminal version (so audit and inspection remain possible
  for the retention window); explicit historical-version reads continue to work until hard-delete.
- `list_policy_versions(policy_id)` — returns the ordered list of versions with `policy_version`, `created_at`,
  `created_by`, `comment`, `version_state`.

**Atomicity.** Version creation, latest-pointer update, and previous-version state transition (`active → superseded` on
update; `active → rolled_back` on rollback; `active → deleted` plus latest-pointer clear on delete) **MUST** commit
atomically. No reader observes intermediate states; any concurrent evaluation either sees the prior version, the new
version, or (post-delete) falls through to the next-most-specific scope, never an inconsistent mix.

**EvaluationContext + Decision diagnostics.** The `EvaluationContext.active_policy` field exposes both `policy_id` and
`policy_version` to Engines. Decision `diagnostics` **MUST** include `engine_id`, `policy_id`, and `policy_version` of
the Policy that produced the Decision so operators inspecting a Decision (via
`cpt-cf-quota-enforcement-fr-evaluate-preview` or via idempotency replay) see exactly which Engine and Policy version
were active at evaluation time. The same three fields are persisted by the idempotency layer per
`cpt-cf-quota-enforcement-fr-idempotency`, so replay Decision diagnostics are identical in shape to live ones.

**Idempotency replay attribution.** Decisions stored in the idempotency cache (per
`cpt-cf-quota-enforcement-fr-idempotency`) retain the original `policy_version`. Replay surfaces the version under which
the original Decision was made, even if the Policy has subsequently been updated to a higher version — the stored
Decision is the source of truth, and forensic analysis can attribute Decisions to specific Policy state.

**Retention.** `superseded`, `rolled_back`, and `deleted` versions are retained for an operator-configurable window
(default: **90 days**) for audit forensics, then hard-deleted by the storage retention sweeper. `active` versions are
never auto-deleted — they remain until explicitly replaced via update, rolled back, or soft-deleted via `delete_policy`.
Cross-reference §6.2 retention table.

**Telemetry.** The system **MUST** expose counters for policy version transitions (total create / update / rollback /
delete) and for `VERSION_CONFLICT` rejections. Label cardinality is a DESIGN-time concern per the existing
telemetry-cardinality discipline.

- **Rationale**: Versioning is the foundation for safe Policy evolution — without it, rollback requires re-editing live
  state (re-introducing race conditions), audit cannot attribute Decisions to specific Policy state, and any future
  shadow-evaluation / promotion / canary mechanism (P2 forward-pointers in §13) lacks a primitive to point at.
  Optimistic concurrency via `if_match_version` keeps the control-plane mutation path simple and avoids the overhead of
  idempotency-record retention for low-frequency operations. Retaining old versions for 90 days serves audit forensics
  without unbounded storage growth.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-manager`

#### Quota Resolution Engine — Pluggable Contract

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-resolution-engine`

The system **MUST** define a **Quota Resolution Engine** plugin contract and **MUST** ship at least two built-in Engines
registered at gear bootstrap: `most-restrictive-wins` (hardcoded; no config; fastest path) and `cel` (sandboxed CEL
evaluator; customizable). Additional Engines (e.g., Starlark, Lua, Wasm-loaded operator engines) are P2-or-later
candidates and **MUST** plug in via the same contract without changes to the multi-quota-evaluation core.

Each Engine **MUST** expose:

- a stable engine_id (matched against Policies' `engine_id` field),
- a config-validation entry point — invoked at Policy create/update; consumes the raw `engine_config`, returns a
  validated/parsed representation or a structured error,
- an evaluate entry point — invoked per operation; receives the EvaluationContext (see below) and the validated config;
  returns a Decision.

The EvaluationContext passed to evaluate **MUST** carry:

- `subjects` — the PDP-authorized attribution set mapped to concrete projections; used for Quota lookup and not exposed
  to CEL,
- `applicable_quotas` — the full list of applicable Quotas with id, subject reference, type, `enforcement_mode` (per
  `cpt-cf-quota-enforcement-fr-enforcement-mode`), cap, period boundary (consumption only), `validity_window` (when set;
  else absent — see `cpt-cf-quota-enforcement-fr-quota-lifecycle`), and the operator-supplied `metadata` map (per
  `cpt-cf-quota-enforcement-fr-quota-metadata`); CEL binds each Quota's value as `arbitration` and MAY pair it against
  `request` (including the canonical `expected_window` key for time-window matching) to implement attribute-based or validity-window-aware
  selection,
- `current_usage` — the current consumed/in-flight amount per applicable Quota,
- `active_policy` — the active Quota Resolution Policy's `policy_id` and `policy_version` (per
  `cpt-cf-quota-enforcement-fr-quota-resolution-policy-versioning`); Engines see the version they are running under so
  audit / replay diagnostics carry it through,
- `request` — the caller-supplied operation-level `metadata` object (forwarded by the
  calling service for debit, reserve, preview, and each batch item; opaque to QE
  core; classified as Platform Operational Data per §6.2 — calling services are responsible for sanitizing values before
  forwarding, since QE persists request metadata in operation logs, idempotency state, and Engine diagnostics where it
  remains visible to authenticated platform actors); this is the request-contract value exposed to CEL,
- `resource` — the optional validated resource projection exposed to CEL, or absent,
- `metric` and `amount` — the operation identity and requested integer amount,
- `time` — the current evaluation timestamp (UTC).

The stable CEL namespaces are `{request, resource, arbitration}`. `request` is the validated operation-level metadata
object, `resource` is the optional validated resource projection, and `arbitration` is the current Quota's validated
constraint object. Authenticated principal data and caller-supplied attribution identifiers **MUST NOT** appear in the
Policy input or affect apportionment, selection, or counter keys.

The Decision returned by the Engine **MUST** match the contract in §3.4: `result`,
`debit_plan: Map<quota_id, QuotaDebitPlan>`, `diagnostics`. The system **MUST** enforce the following Debit-Plan
invariants on every returned Decision; any violation **MUST** surface a platform-canonical error (fail-closed) with no
counter mutation, and the Decision shape **MUST NOT** be returned to the caller:

1. Every quota_id in `debit_plan` **MUST** be a member of `applicable_quotas`. The Engine cannot invent or reference
   Quotas outside the EvaluationContext.
2. For every entry: `amount ≥ 0`.
3. For every entry: `amount ≤ request.amount`. No single counter is charged more than the operation requested. This
   prevents accidental per-Quota over-charge while permitting both sum-semantics (one operation distributed across
   pools: `sum(amount) = request.amount`) and multi-counter / AND-semantics (each applicable pool tracks the operation
   independently: `sum(amount) = N × request.amount` for N entries) — the choice is Engine-driven, not core-mandated.
4. Result-plan consistency:
   - `result = Allowed` ⇒ `debit_plan` is non-empty.
   - `result = Denied` ⇒ `debit_plan` is empty.

The system intentionally does **not** constrain `sum(amount)`. Cascade and proportional-split
Engines naturally produce `sum(amount) = request.amount`; AND-across-tiers Engines naturally produce `sum(amount) = N × request.amount`;
clamp-style admission (where the Engine admits fewer units than requested, e.g., the future `hard-with-clamp`
enforcement mode tracked in §13) naturally produces `sum(amount) < request.amount` paired with an appropriate non-`Allowed`
result variant — all expressible within the existing invariants without future schema relaxation. Engines that produce
sparse Debit Plans (e.g., cascade) commit to integer per-Quota magnitudes; rounding choices in CEL or future Engines
are the Engine author's responsibility.

Engines **MUST** be deterministic given the EvaluationContext (the system relies on this for idempotent replay). Engines
**MUST NOT** perform I/O. The system **MUST** enforce a per-Policy evaluation timeout (default 5ms, see Quota Resolution
Policy FR); on timeout, the system surfaces a canonical `DeadlineExceeded` error (fail-closed) and the Engine's partial
Decision (if any) is discarded.

Engine plugins are registered in-process at gear bootstrap. Adding a new Engine to a deployment requires building a
gear binary that includes the Engine; runtime registration of arbitrary user-supplied Engines is out of scope.

**Bootstrap failure is fail-fast.** If any built-in Engine declared in the deployment manifest fails to register at
gear bootstrap (e.g., the `cel` evaluator throws during initialization, a future Wasm-loaded Engine fails to compile,
the Engine binary version mismatches the QE core version), the gear **MUST** fail readiness and **MUST** refuse to
serve requests. The system **MUST NOT** silently fall back to a different Engine for Policies that referenced the failed
one — silent fallback is unsafe because operator Policies referencing the unavailable Engine would change behavior
without operator awareness, potentially widening enforcement (a CEL Policy that denied is replaced by
`most-restrictive-wins` that allows). Failed-bootstrap state **MUST** surface a structured log entry and a telemetry
counter `engine_bootstrap_failures_total` so operators can diagnose without reading log files. Recovery requires fixing
the registration failure and restarting the gear.

##### P1 Built-in Engines

The system **MUST** ship the following two built-in Engines in P1; both **MUST** be registered automatically at gear
bootstrap:

(1) `most-restrictive-wins` — hardcoded for maximum throughput; rejects any non-empty `engine_config`. Behavior:

- **Empty applicable-Quotas set**: `result = Denied(violated_quota_ids=[], reason="NO_APPLICABLE_QUOTA")` — for a
  `QuotaGated` metric, absence of any applicable Quota means absence of authorization (per §3.2). `debit_plan` is empty.
- **Validity-window prefilter (default)**: Quotas whose `validity_window` is set and whose `time` falls outside
  `[validity_start, validity_end]` are excluded from consideration before the cap comparison runs (per
  `cpt-cf-quota-enforcement-fr-quota-lifecycle` validity-window semantics). Quotas without a `validity_window` field are
  always considered. If every applicable Quota is excluded by the prefilter, the post-prefilter set is empty and the
  result is `Denied(violated_quota_ids=[], reason="NO_APPLICABLE_QUOTA")` — same outcome as if no Quotas existed.
  Operators wanting a softer "out-of-window Quotas don't deny but also don't constrain" outcome express it through a
  `cel` Policy.
- **Metadata is ignored.** This Engine does not read `arbitration` or `request`. Operators who need
  metadata-driven Quota selection (region-gating, tier-gating, environment-gating per
  `cpt-cf-quota-enforcement-fr-attribute-based-quota-selection`) **MUST** configure a `cel` Policy on the affected
  metric.
- **Binding-Quota selection.** A Quota is satisfiable if its remaining capacity is ≥ `request.amount` (unbounded Quotas
  — `cap = null` — are trivially satisfiable, since remaining is infinite). The binding Quota is selected from the
  satisfiable set by, in priority order:
  1. **Subject-scope tier** — more-specific owner projection wins. P1: user-scope > tenant-scope. Additional owner
     scopes require an explicit catalogue and priority decision.
  2. **Bounded > unbounded** within the chosen tier — operator's explicit cap takes precedence; unbounded becomes
     binding only as overflow when no bounded satisfiable Quota exists in the tier.
  3. **Smallest remaining capacity** within bounded satisfiable Quotas of the chosen tier (literal "most restrictive");
     ties broken by ascending `quota_id` (UUIDv7). Among unbounded Quotas (reached only when rule 2 falls through),
     ascending `quota_id` is the sole tiebreaker.
- `result = Denied(violated_quota_ids, …)` when no Quota is satisfiable — every applicable bounded Quota has remaining
  capacity below `request.amount` and no applicable unbounded Quota exists. `violated_quota_ids` names every such
  bounded Quota (no short-circuit; full enumeration). Unbounded Quotas are never violators.
- else `result = Allowed`. `debit_plan` is a single entry against the binding Quota at `amount = request.amount`;
  non-binding applicable Quotas are absent from the plan and their counters are not mutated.
- **Engine-specific invariant (stricter than the general Engine contract):** on `result = Allowed` the Debit Plan
  **MUST** be exactly one entry with `amount = request.amount`. The system enforces this at the Engine boundary in
  addition to the general per-entry `0 ≤ amount ≤ request.amount` contract; any deviation surfaces a canonical
  `Internal` error.

(2) `cel` — sandboxed CEL evaluator. Config: `{ expr: <CEL string> }`. Behavior:

- Validation (Policy create/update): parse + type-check expression against the EvaluationContext, registered projection
  and constraint schemas, pair compatibility, and Decision return schema; reject errors with line/column.
- Per operation: bind the EvaluationContext into the CEL environment; evaluate the expression under sandbox (no I/O;
  deterministic; fixed step/cost cap, default tuned to the per-Policy timeout); interpret the returned record as a
  Decision and apply the standard Debit-Plan invariants.
- Runtime errors (cost-cap exceeded, type error at evaluation, malformed return record) **MUST** surface a
  platform-canonical error (per §3.4 failure surface) with no counter mutation.

P2 Engine candidates (Starlark, Lua, Wasm) plug in via the same contract; selection criterion: sandboxability (no I/O),
determinism, predictable resource limits, fast startup. The choice is deferred to a later PRD update once production CEL
coverage is observed.

- **Rationale**: A pluggable Engine contract treats arbitration logic the same way the platform already treats storage
  and notification — as a versioned trait with a small set of in-tree implementations and room for future ones. Strict
  Debit-Plan invariants enforced at the Engine boundary mean malformed Engines (including future bugs in
  operator-authored CEL) cannot corrupt counters; the system always converts invariant violations into a canonical
  `Internal` error rather than partial mutations. P1 ships both a fastest-possible hardcoded path
  (`most-restrictive-wins`) and a customizable path (`cel`) so cascade and similar arbitration patterns are expressible
  from day one without sacrificing the default-path latency budget.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Cascade Arbitration Capability

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-cascade`

The Engine layer (per `cpt-cf-quota-enforcement-fr-quota-resolution-engine`) **MUST** be expressive enough for cascade
arbitration — directing debits at one Quota (the "primary pool") first and falling through to another (the "fallback
pool") only when the primary's remaining capacity is insufficient. P1 ships two complementary capabilities:

- **Built-in subject-scope cascade** — the default `most-restrictive-wins` Engine cascades through subject-scope tiers
  (P1: user-scope > tenant-scope) with single-entry Debit Plans. No operator authoring required.
- **Customizable cascade** — the `cel` Engine produces arbitrary multi-entry Debit Plans expressing cross-tier
  **splits** (primary takes `X`, fallback takes `amount − X`), intra-tier cascade between same-scope Quotas identified
  by metadata, proportional distributions, and multi-tier (3+ pool) cascades. Standard Debit-Plan invariants from
  `cpt-cf-quota-enforcement-fr-quota-resolution-engine` apply uniformly.

Future Engine plugins added to QE **MUST** preserve at least the customizable cascade expressiveness.

**Reference scenario.** For the `llm_gateway` owner's AI-token metric, tenant T owns an
`(…llm_gateway.user.v1~, U)` Quota with `cap=100, remaining=20` and an
`(…llm_gateway.tenant.v1~, T)` Quota with `cap=10000, remaining=9700`. Resolution yields both owner scopes for one
debit of 50:

- `most-restrictive-wins`: `user_q` not satisfiable (20 < 50); cascade falls through to `tenant_q`. Result: `Allowed`,
  `debit_plan = { tenant_q.id: { amount: 50 } }`; `user_q.remaining` stays at 20.
- `cel` split-cascade Policy: `user_q` contributes its full remaining; `tenant_q` covers the residual. Result:
  `Allowed`, `debit_plan = { user_q.id: { amount: 20 }, tenant_q.id: { amount: 30 } }`.

The split-vs-fallthrough difference appears only when the primary has some but insufficient remaining; with
`user.remaining = 0`, both Engines produce the same single-entry plan against the fallback pool.

- **Rationale**: Cascade is the most operator-requested non-default arbitration pattern (license-pack workflows:
  per-user pool first, tenant top-up pool as fallback). The default `most-restrictive-wins` covers the common case
  without operator authoring; `cel` adds split-cascade and proportional-distribution for license-overage and
  multi-tier billing workflows. Declaring both as a first-class **capability obligation** on the Engine layer anchors
  operator expectations and prevents future Engine candidates from dropping the expressiveness needed.

- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Attribute-Based Quota Selection (Metadata-Gated)

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-attribute-based-quota-selection`

Operators frequently need attribute-based Quota selection — region-gating, tier-gating, or environment-gating, where
each operation is directed at the Quota whose attributes match the operation's context. This is **operator-authored
Policy logic** carried in an Engine's `engine_config`; QE does not interpret metadata keys or values and embeds no
matching grammar. The role of this FR is to guarantee the **primitives** required to express the case through a Policy.

The system **MUST** guarantee:

- **Validated attributes reach the Engine verbatim.** Each applicable Quota's `metadata` (per
  `cpt-cf-quota-enforcement-fr-quota-metadata`) is exposed to CEL as `arbitration`, and the caller-supplied operation
  `metadata` as `request`. QE validates shape at the
  prescribed boundary but does not interpret business meaning.
- **Subject resolution is metadata-agnostic.** `applicable_quotas` always contains every Quota whose
  `(projection_type, subject_id)` matches subject resolution, regardless of metadata. Multiple Policies at the same scope
  can therefore use different matching rules without colliding at the resolution layer.
- **The P1 `cel` Engine can express matching predicates over both metadata maps** and emit a sparse Debit Plan over
  whichever subset the operator's predicate selects.

Standard Debit-Plan invariants from `cpt-cf-quota-enforcement-fr-quota-resolution-engine` apply: `Allowed` with an empty
`debit_plan` for non-zero `request.amount` is invalid and **MUST** surface a canonical `Internal` error (fail-closed).
Policies that filter out every applicable Quota **MUST** therefore return `Denied` with an actionable `reason`;
operators wanting unconstrained consumption should reclassify the metric as `Direct` in `types-registry` rather than
emit `Allowed` with an empty plan.

Worked example (illustrative — Policy logic is operator-authored). Tenant T owns user U; operator wants per-region caps
for metric `gpu-hours`:

- Quotas on `(owner-user-projection, U)`: `Q_us(metadata={regions:["us-east-1"]}, cap=100)`,
  `Q_eu(metadata={regions:["eu-west-1"]}, cap=50)`.
- Operator-authored `cel` Policy on `(metric=gpu-hours)` with predicate
  `request.region in arbitration.regions`.

`debit(metric=gpu-hours, amount=20, metadata={region:"us-east-1"})`:

- `applicable_quotas = {Q_us, Q_eu}` (metadata-agnostic),
- Engine's predicate matches `{Q_us}`; returns `Allowed` with `debit_plan = { Q_us.id: { amount: 20 } }`,
- `Q_us.consumed += 20`; `Q_eu` untouched.

Registered contracts guarantee referenced required keys are present and correctly typed. Agreement between the request
and Quota sides — compatible types/domains and the correct scalar/collection operator — is checked when the Policy is
saved. Runtime business mismatch remains Policy-defined and typically returns `Denied` with an actionable reason.

Policies **MUST NOT** branch, apportion shared budgets, select Quotas, or construct Debit Plans from the authenticated
service principal or the target tenant/subject identifiers. Those fields are authorization and lookup inputs, not
arbitration data.

The default Policy `most-restrictive-wins` (per §5.9) **does not read metadata** — under it, the binding Quota is
debited regardless of attributes, and the non-binding applicable Quotas (whether or not their metadata matches the
request) are untouched. Operators who depend on attribute-based selection **MUST** opt into a `cel` Policy on the
affected metric.

- **Rationale**: Region-, tier-, and environment-gating are the most-requested attribute-based patterns in production
  deployments. Exposing only primitives — never built-in matching semantics — preserves Engine pluggability, avoids
  ossifying any specific matching grammar, and lets operators compose attribute-based selection with cascade and other
  patterns in a single Policy. Metadata-agnostic subject resolution means future projection scopes (resource-group
  hierarchy, P3) inherit the capability without any QE-side change.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-consumer`

### 5.10 Quota Snapshot Read API

#### Quota Snapshot Read for a Subject

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-quota-snapshot-read`

The system **MUST** provide an S2S read API that returns, for explicit PDP-authorized tenant/subject/metric filters, a
**Quota Snapshot**: the list of every applicable Quota with its current per-Quota state. Catalogue mapping applies per
`cpt-cf-quota-enforcement-fr-subject-resolution`; the response is engine-agnostic — no aggregate "headline" cap or
balance is computed, since under cascade, split, or attribute-gated Engines no single number is universally meaningful.
Callers needing an admission verdict for a specific operation **MUST** use the read-only Decision Preview
(`cpt-cf-quota-enforcement-fr-evaluate-preview`), which runs the active Engine and returns the would-be `Decision` and
`debit_plan`.

For each applicable Quota the response **MUST** include:

- `quota_id`, subject reference, metric, quota type (`allocation` / `consumption`),
- `cap` (numeric or `null` for unbounded Quotas per `cpt-cf-quota-enforcement-fr-quota-lifecycle`), current consumed (or
  in-flight, for allocation), `remaining` (numeric, or `null` when `cap` is `null`),
- `enforcement_mode` (per `cpt-cf-quota-enforcement-fr-enforcement-mode`),
- the period boundary and next reset timestamp (consumption types only) or "no period" (allocation types),
- the Quota's `metadata` map (subject to PDP scoping),
- `validity_window` (when set) plus a server-computed boolean `currently_within_window` so callers can render expiry
  state without recomputing the comparison.

The response **MUST NOT** carry Quota Resolution Policy attribution (no `policy_id`, `policy_version`, `scope`,
`engine_id`, `engine_config`, or any summary/content-hash thereof). Snapshot is per-Quota state only. Callers needing to
attribute the read to a specific Policy state **MUST** use `cpt-cf-quota-enforcement-fr-evaluate-preview` (whose
`diagnostics` carry `policy_id` and `policy_version` per
`cpt-cf-quota-enforcement-fr-quota-resolution-policy-versioning`) or the Policy-read API exposed by the same versioning
FR.

- **Rationale**: Returning the per-Quota list, rather than an aggregate "effective cap", keeps the read engine-agnostic
  and avoids baking `most-restrictive-wins` semantics into the data model. UIs and dashboards compose the display they
  need (single progress bar of the minimum, multi-row breakdown, cascade-aware "user pool first, then tenant", etc.) on
  top of the same primitive. The authoritative "would this operation be admitted?" question is answered by the Decision
  Preview, which exercises the active Engine instead of guessing.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`, `cpt-cf-quota-enforcement-actor-quota-reader`

#### Bulk Quota Snapshot Read

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-bulk-quota-snapshot-read`

The system **MUST** support a bulk Quota Snapshot read API that returns the per-Quota state for multiple
`(subject, metric)` pairs in a single call, subject to the caller's PDP-authorized scope. Bulk reads **MUST** be
paginated when the result set exceeds an operator-configured page size (default: 100 entries per page) and **MUST**
support cursor-based continuation.

- **Rationale**: Dashboards and billing systems need to read quota state for many subjects at once; per-subject calls do
  not scale and break latency budgets.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-reader`

#### Consumer-Backed Self-Service Quota Snapshot Read

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-end-user-quota-snapshot-read`

The system **MUST** keep the Quota Snapshot consumer surface service-to-service. A consuming product backend may use it
to serve an end-user view by supplying explicit tenant/user attribution authorized against its authenticated service
principal. End users **MUST NOT** call QE directly. The endpoint **MUST**:

- return only Quotas whose mapped subjects match the PDP-authorized supplied tenant/user tuple — no cross-user or
  cross-tenant Quotas,
- return **every** applicable Quota under that scope — quotas that govern a subject's consumption are transparent to
  that subject,
- return the same per-Quota state contract as `cpt-cf-quota-enforcement-fr-quota-snapshot-read` (the only difference
  between the two endpoints is the applicable-Quotas filter described above; the per-Quota state shape is identical),
- carry no Quota Resolution Policy attribution — same rule as the base snapshot; Policy-attribution callers route
  through `cpt-cf-quota-enforcement-fr-evaluate-preview` or the Policy-read API,
- authorize the complete explicit target against the authenticated service principal before catalogue mapping or storage.

End-user-facing UI surfaces (web, mobile, CLI) call their product backend, not Quota Enforcement directly. This PRD
does not specify that product's end-user authentication or rate-limit story.

- **Rationale**: Letting users observe their own quota state without going through a tenant administrator is fundamental
  to a self-service platform; that surface lives in the consuming product. The read is tenant-isolated because PDP
  authorizes the supplied target against the backend's service principal. Quotas are transparent to the subjects they
  govern: every Quota applicable to the authorized owner user/tenant projection set is surfaced. PDP scoping is the
  only partitioning mechanism — operator-level grants see across tenants/subjects, consumer and tenant-admin grants see
  only their own subject set; no per-Quota or per-key invisibility primitive exists.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

### 5.11 Quota Enforcement Mode

#### Enforcement Mode Closed Enum

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-enforcement-mode`

Every Quota **MUST** carry an `enforcement_mode` field drawn from GTS instances under base
`gts.cf.qe.enforcement.type.v1~`. **P1 reserves one instance**:
`gts.cf.qe.enforcement.type.v1~cf.qe.enforcement.hard.v1` (`hard`) — operations whose execution would cause a
`hard` Quota's remaining capacity to drop below zero are denied per `cpt-cf-quota-enforcement-fr-hard-quota-reject`;
counter integrity is preserved (the counter never crosses cap).

> **Notation.** Throughout this document, enforcement modes are referenced by their short instance names (`hard`,
> future `hard-with-clamp`) in prose for readability. API requests, storage rows, and outbox events use the full GTS
> URI form shown above.

The field is anchored to the GTS instance form so future modes are addable as new instances without API breakage.
Future phases MAY extend additively:

- `hard-with-clamp` (P3 candidate, see §13) — for batch-style workloads where admitting a clamped magnitude is
  preferable to rejecting outright. Requires registering a new instance
  `gts.cf.qe.enforcement.type.v1~cf.qe.enforcement.hard_with_clamp.v1` and the Decision contract to gain an
  `AllowedWithClamp(quota_id, admitted_magnitude)` arm; the existing per-entry `amount ≤ request.amount` Debit-Plan
  invariant already accommodates clamped magnitudes without further relaxation.

- **Rationale**: A closed enum centralizes the "what does this Quota do at the cap?" decision in a single field rather
  than scattering it across ad-hoc boolean flags. The P1 single-value enum keeps the model simple while reserving design
  space for the cap-clamp future extension without forcing it now. Commercial overage pricing is intentionally deferred
  to the billing layer per §4.2 — billing composes Usage Collector observations with Quota records to compute over-cap
  consumption without QE itself admitting over-cap operations.

- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-quota-consumer`

#### Cap Violation Rejection

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-hard-quota-reject`

The system **MUST** deny any operation whose execution would cause a Quota's remaining capacity to drop below zero
(`hard` being the only `enforcement_mode` value in P1; see `cpt-cf-quota-enforcement-fr-enforcement-mode`). The denial
decision **MUST** identify each violating Quota by quota ID, the requested amount, the current remaining capacity, and
the violation amount. Counters **MUST NOT** be modified for denied operations.

- **Rationale**: Cap-violation rejection is the P1 strict-enforcement primitive; its violation must be unambiguous and
  auditable. Multi-Quota evaluation (§5.9) ensures every violating Quota is named (no short-circuit on the first
  violation) so callers and audit consumers see the full denial reason.
- **Actors**: `cpt-cf-quota-enforcement-actor-quota-consumer`

### 5.12 Tenant Isolation

#### Strict Tenant Isolation on All Operations

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-tenant-isolation`

The system **MUST** enforce strict tenant isolation on every read and write operation against per-tenant entities. Every
Quota, counter, lease, idempotency record, and operation log entry **MUST** carry the PDP-authorized target `tenant_id`
at write time. Reads **MUST** bind that authorized tenant plus PDP-returned scope at storage; cross-tenant read or write
operations **MUST** be rejected, with the only exception being explicit
operator-level PDP grants for platform-wide operator activities (e.g., aggregated utilization dashboards).

Operator-managed platform-wide entities — Quota Resolution Policies, registry-resident projection contracts, and registered Quota
Resolution Engines — are **not** tenant-scoped. Their addressing uses scope discriminators specific to each entity
(e.g., a Policy's `scope` field per `cpt-cf-quota-enforcement-fr-quota-resolution-policy`) rather than a `tenant_id`
column. Mutation of these entities is gated by operator-level PDP grants and is out of scope for tenant isolation
enforcement.

- **Rationale**: Tenant isolation is a baseline security and compliance requirement (REQ.md §9). Tagging every persisted
  row with `tenant_id` at write time makes isolation enforceable at the storage layer rather than only at the API
  boundary.
- **Actors**: All actors

### 5.13 Authorization

#### PDP-Gated Authorization

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-authorization`

The system **MUST** authorize every read and write operation via the platform PDP (`authz-resolver`). Authorization \*
*MUST*\* verify that the caller's authenticated identity (from SecurityContext) is permitted to perform the requested
operation against the targeted subject and metric. PDP-returned constraints (e.g., scoping a read to specific tenants)
\* *MUST*\* be applied as additional filters before query execution. Authorization failures **MUST** result in immediate
rejection with no partial operation; the system **MUST** fail closed (see §3.4 — "Quota Enforcement itself fails closed
on internal errors").

Consumer operations **MUST** supply the target `tenant_id`, subject refs, metric, and optional resource. QE **MUST**
reject malformed public request/target shape before authorization, send the complete structurally valid untrusted tuple to PDP,
and proceed only when PDP authorizes it for the authenticated service principal. Catalogue and contract validation
**MUST** occur only after authorization. Management operations likewise carry an explicit target and apply PDP-returned
scope. End-user principals never call QE consumer endpoints directly.

The authenticated service principal is the caller identity; QE **MUST NOT** accept a second self-declared caller
identity. Neither principal data nor attribution identifiers may influence Policy branching, quota apportionment,
Quota selection, Debit Plans, or counter/allocation keys beyond selecting the authorized subjects' Quotas.

- **Rationale**: Centralizing authorization in the platform PDP keeps policy decisions out of the data plane — operators
  adjust permissions without redeploying QE, and authz logic stays under a single audit boundary in `authz-resolver`.
  Requiring PDP to authorize the complete supplied attribution tuple is what makes tenant isolation enforceable: a
  misconfigured or compromised service cannot charge another tenant merely by changing request fields. Failing closed on PDP
  unavailability prevents an outage from degrading into a permissive bypass. The same discipline is applied by Usage
  Collector (`cpt-cf-usage-collector-fr-tenant-attribution`, `cpt-cf-usage-collector-fr-ingestion-authorization`) for the same
  reasons.
- **Actors**: `cpt-cf-quota-enforcement-actor-authz-resolver`

### 5.14 Pluggable Storage Backend

#### Pluggable Storage Backend Contract

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-pluggable-storage`

The system **MUST** define a storage plugin contract that abstracts the persistence layer for Quotas, counters, leases,
Quota Resolution Policies, idempotency records, and operation logs. The contract **MUST** expose primitives sufficient
to support transactional debit, credit, rollback, reserve, commit, and release operations with strong consistency within
a single tenant scope. P1 ships a single plugin implementation built on `toolkit-db`; alternative backends are viable
under the same plugin contract. The operator selects the active plugin via configuration.

The plugin contract **MUST** be versioned with the gear's major version; plugins implementing previous contract
versions are not supported in newer gear versions.

- **Rationale**: Pluggability avoids storage lock-in and lets future deployments adopt different backends (e.g.,
  distributed key-value stores for higher throughput) without changing the Quota Enforcement gateway. P1 commitment to a
  single `toolkit-db`-based plugin keeps the initial scope focused.
- **Actors**: `cpt-cf-quota-enforcement-actor-platform-operator`, `cpt-cf-quota-enforcement-actor-storage-backend`

### 5.15 Notification Plugin Contract

#### Notification Plugin Contract & Event Catalog

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-notification-plugin`

The system **MUST** define a notification plugin contract that allows deployment-specific event sinks to receive
structured notification events. The plugin contract **MUST** be an in-process Rust trait in P1; P2 introduces a
standardized routing layer through the platform EventBus. The system **MUST** support multiple registered plugins per
deployment; events are dispatched to all registered plugins.

The system **MUST** emit events for the following catalog of event kinds:

- `threshold-crossed` — a Quota's actual `consumed` amount crossed at least one operator-configured threshold (e.g.,
  50%, 80%, 100% of cap) on an **upward transition**. The system tracks the highest threshold crossed so far per
  `(Quota, period)`; emission fires only when a successful counter mutation moves `consumed` strictly above that marker.
  **One** `threshold-crossed` event is emitted per such transition; its payload carries
  `crossed_thresholds: [<list of every threshold the mutation crossed, ascending>]` and `highest_crossed_threshold` (the
  maximum of that list). `Denied` outcomes and canonical-error responses do **NOT** emit `threshold-crossed` — counters
  were not modified, so no actual threshold transition occurred. The marker resets at period rollover so thresholds can
  fire again in the new period,
- `period-rollover` — a consumption-type Quota crossed a period boundary, with closing-period consumed amount,
  closing-period cap, and the new period boundary,
- `lease-auto-released` — a lease TTL expired without commit/release,
- `lease-resolved-by-deactivation` — a lease was atomically resolved when its Quota was deactivated (one event per
  affected lease; emitted from `cpt-cf-quota-enforcement-fr-quota-lifecycle` deactivation flow),
- `quota-changed` — a Quota was created, updated, or deactivated,
- `quota-counter-adjusted` — a credit was applied to a Quota's counter outside the natural debit/rollback flow (e.g.,
  redistribution between subjects, SLA-breach grant, manual adjustment by Quota Manager), carrying the credited amount,
  the target Quota, and the authenticated service principal. Fires only for `credit`
  (`cpt-cf-quota-enforcement-fr-credit`); rollback uses the dedicated `quota-rollback-applied` event below,
- `quota-rollback-applied` — a previously committed debit was reversed via the rollback primitive
  (`cpt-cf-quota-enforcement-fr-rollback`). Carries the original debit's idempotency key, the rolled-back amount, the
  target Quota, and the consumer identity from the SecurityContext. Distinct from `quota-counter-adjusted`: rollback
  reverses a specific prior debit, while `quota-counter-adjusted` describes an unsolicited compensation (credit). Fires
  on every successful rollback regardless of whether the underlying debit originated from a direct `debit` call or a
  lease `commit`,
- `policy-changed` — a Quota Resolution Policy was created, updated, or deleted.

Each event **MUST** carry `event_id`, `event_kind`, `tenant_id`, `quota_id` or `policy_id` (whichever applies),
`subject` (when applicable), event-specific payload, and an emission timestamp. Event delivery is best-effort in phase 1
— sustained delivery failures **MUST** be reflected in operational telemetry but **MUST NOT** block Quota Enforcement
write operations. Delivery MAY duplicate under retry; sinks **MUST** tolerate duplicate delivery of the same
`event_id`.

- **Rationale**: Threshold-based notifications are a recurring requirement across consuming services — without a shared
  primitive every team reimplements them inconsistently. Centralizing the catalog and emission contract in QE eliminates
  that duplication. Plugin pluggability bridges the gap until the platform EventBus is available.
- **Actors**: `cpt-cf-quota-enforcement-actor-notification-sink`, `cpt-cf-quota-enforcement-actor-platform-operator`

### 5.16 Operational Telemetry

#### Gear-Specific Telemetry

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-fr-telemetry`

The system **MUST** follow platform observability conventions for the baseline observability surface — HTTP request
counts and latencies, health endpoints, OpenTelemetry traces — provided by `toolkit-observability` and the `api-gateway`
framework. On top of that baseline, the system **MUST** expose the following gear-specific counters, histograms, and
gauges that surface QE-internal policy decisions and guard-rail rejections invisible to the framework baseline:

- **`denial_total`** — count of admission denials by closed reason kind.
- **`lease_contention_rejected_total`** — acquisitions rejected due to acquisition contention timeout per
  `cpt-cf-quota-enforcement-fr-lease-acquire`, by canonical registered `metric`.
- **`lease_acquisition_wait_seconds`** — histogram of wait times during lease acquisition (both successful waits and
  rejected ones), by canonical registered `metric`.
- **`lease_inflight_limit_exceeded_total`** — acquisitions rejected by the per-`(tenant, metric)` active-lease cap per
  `cpt-cf-quota-enforcement-fr-lease-timeout`, by canonical registered `metric`.
- **`lease_unreclaimed_expired`** — count of expired-but-unreclaimed leases, by canonical registered `metric`.
- **`engine_bootstrap_failures_total`** — Engine registration failures at gear bootstrap per
  `cpt-cf-quota-enforcement-fr-quota-resolution-engine`.
- **`engine_evaluation_seconds`** — Engine evaluation latency.
- **`policy_version_transitions_total`** — Policy-version create, update, rollback, and delete transitions, labelled by
  `transition_kind` from the closed set `{create, update, rollback, delete}`.
- **`policy_version_conflict_rejections_total`** — Policy writes rejected with `VERSION_CONFLICT`; no labels.
- **`debit_plan_invariant_violations_total`** — Decisions rejected for violating Debit-Plan invariants per
  `cpt-cf-quota-enforcement-fr-quota-resolution-engine`, labelled by invariant from the closed set
  `{quota_id_outside_applicable_set, negative_amount, amount_exceeds_request_amount, result_plan_inconsistency}`.
- **`quota_cap_zero_total`**, **`quota_cap_unbounded_total`** — gauges of the Quotas with lifecycle status `active`
  whose `cap = 0` and `cap = null` respectively, whatever their validity window, per
  `cpt-cf-quota-enforcement-fr-quota-lifecycle` cap value semantics (operator misconfiguration surfaces).
- **`quota_for_direct_metric_total`** — gauge of the Quotas with lifecycle status `active` whose metric is currently
  classified `Direct` in `types-registry` per the §3.2 inertness rule (surfaces operator misconfigurations where a
  Quota was declared on a non-gated metric); deactivation and a mode change take effect at the next refresh.
- **`contract_validation_failures_total`** — rejected contract instances by closed validation surface/reason.
- **`admitted_metric_violations_total`** — projection/metric incompatibilities by closed validation surface.
- **`notification_dispatch_failures_total`** — per-sink dispatch failures, by `sink_id` and `event_kind`.
- **`outbox_pending_rows`** — outbox backlog gauge, by `queue`.
- **`outbox_rejections_total`** — handler `Reject` outcomes, by `queue`; this is not a durable dead-letter row count.

Labels **MUST NOT** include high/unbounded-cardinality identifiers (`tenant_id`, `subject_id`, `quota_id`, `policy_id`,
`idempotency_key`, `lease_token`), projection type, caller attribution, or raw/unregistered metric input. Permitted
dimensions are deployment-bounded values — canonical registered `metric` only on instruments that declare it above,
`engine_id`, `operation`, `transition_kind`, validation `surface`, `invariant`, closed `reason` enums, `sink_id` (the
deployment-registered sink set), `event_kind` (the closed event catalog), and `queue` (the fixed outbox queue set). A
`metric` label **MUST** be populated only after registry/catalogue validation and **MUST** use the canonical registered
identity; unknown or unadmitted caller input **MUST NOT** appear as a label. Caller attribution is retained on sampled
traces/diagnostics, not metrics.

- **Rationale**: The framework baseline already covers HTTP-server-level observability and health endpoints uniformly
  across gears; restating it here would duplicate convention. The instruments above expose policy-decision and
  guard-rail signals unique to QE and invisible to the framework — they are the difference between "gateway is up" and
  "quota arbitration is healthy". Bounding label cardinality at the PRD level prevents a 100M-subject deployment from
  creating per-tenant time series that exhaust the metrics backend, while selected registered-metric labels remain
  bounded by the deployment catalogue and preserve operational localization.
- **Actors**: `cpt-cf-quota-enforcement-actor-monitoring-system`, `cpt-cf-quota-enforcement-actor-platform-operator`

## 6. Non-Functional Requirements

### 6.1 Gear-Specific NFRs

#### Evaluation Latency

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-evaluation-latency`

Single-quota evaluation operations (debit, credit, rollback, reserve, commit, release, evaluate_preview, Quota Snapshot
read for one subject/metric pair) **MUST** complete within 100ms at p95 under target load.

- **Threshold**: p95 ≤ 100ms at 10 000 ops/sec sustained
- **Rationale**: Quota evaluation sits on the hot path of every consuming-service request; latency above this budget
  breaks consumer SLOs.
- **Architecture Allocation**: See DESIGN.md

#### Throughput

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-throughput`

The system **MUST** sustain at least 10 000 evaluation operations per second under normal operation across all tenants
combined.

- **Threshold**: ≥ 10 000 ops/sec sustained
- **Rationale**: High-volume services (LLM Gateway, API Gateway) generate significant per-request quota checks; the
  evaluation path must not become a bottleneck.
- **Architecture Allocation**: See DESIGN.md

#### Subject Scale

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-subject-scale`

The system **MUST** support at least 100 000 000 distinct subjects (combined `tenant` and `user` subjects) without
degrading evaluation latency below the `cpt-cf-quota-enforcement-nfr-evaluation-latency` threshold.

- **Threshold**: ≥ 100M subjects supported
- **Rationale**: CF/Gears are targeted at multi-tenant deployments with very large user populations; subject-scale
  must not be the limiting factor.
- **Architecture Allocation**: See DESIGN.md

#### Quota Density per Subject

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-quota-density`

The system **MUST** support at least 10 active Quotas per subject without degrading evaluation latency below the
`cpt-cf-quota-enforcement-nfr-evaluation-latency` threshold. Combined with subject scale, the system supports at least 1
billion total active Quotas.

- **Threshold**: ≥ 10 Quotas per subject; ≥ 1B total active Quotas
- **Rationale**: Each consuming service introduces its own quota dimensions; subjects in production routinely accumulate
  many distinct Quotas as the platform grows.
- **Architecture Allocation**: See DESIGN.md

#### High Availability

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-availability`

The system **MUST** maintain 99.95% monthly availability for evaluation endpoints.

- **Threshold**: 99.95% uptime per calendar month
- **Rationale**: Quota evaluation is on the critical path for every guarded operation; downtime translates directly to
  consuming-service downtime.
- **Architecture Allocation**: See DESIGN.md

#### Authentication Required

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-authentication`

The system **MUST** require authentication for all API operations. Unauthenticated requests **MUST** be rejected before
any operation is performed.

- **Threshold**: Zero unauthenticated API access
- **Rationale**: Quota state is tenant-sensitive; unauthenticated access is a security violation.
- **Architecture Allocation**: See DESIGN.md

#### Authorization Enforcement

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-authorization`

The system **MUST** enforce authorization for all read and write operations via the platform PDP, fail closed on PDP
unavailability or denial, and apply PDP-returned constraints as additional query filters before query execution.

- **Threshold**: Zero unauthorized data access or write
- **Rationale**: Authorization prevents unauthorized counter mutation and cross-tenant data leakage.
- **Architecture Allocation**: See DESIGN.md

#### Tenant Isolation Integrity

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-tenant-isolation-integrity`

The system **MUST** ensure zero cross-tenant data leakage in any read or write operation; storage-layer enforcement and
PDP authorization **MUST** independently enforce tenant boundaries.

- **Threshold**: Zero cross-tenant read or write events in soak testing and chaos testing
- **Rationale**: Tenant isolation breaches are an existential platform risk; defense in depth is required.
- **Architecture Allocation**: See DESIGN.md

#### Idempotency Guarantee

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-idempotency-guarantee`

The system **MUST** guarantee that every write operation with an idempotency key produces exactly one counter effect
regardless of the number of replays, under at-least-once delivery from upstream callers.

- **Threshold**: Zero double-count events under simulated retry storms (10× normal RPS, 5% retry rate)
- **Rationale**: Idempotency is the single most important correctness property for a counter/ledger system; violations
  corrupt billing-relevant state.
- **Architecture Allocation**: See DESIGN.md

#### Storage Fault Tolerance & RPO

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-fault-tolerance`

The system **MUST** guarantee that every write operation that has returned `Allowed` or `Denied` to the caller is
durably persisted; no committed counter mutation may be lost due to storage backend restarts, gateway restarts, or
single-node failures.

- **Threshold**: RPO = 0 for committed operations (zero committed-data loss)
- **Rationale**: Quota counters are billing-relevant; lost mutations create reconciliation risk and tenant complaints.
- **Architecture Allocation**: See DESIGN.md

#### Recovery Time

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-nfr-recovery`

The system **MUST** resume normal evaluation operations within 15 minutes of storage backend recovery from an outage.

- **Threshold**: RTO ≤ 15 minutes from storage backend recovery
- **Rationale**: Bounded recovery time prevents extended platform-wide unavailability when storage outages occur.
- **Architecture Allocation**: See DESIGN.md

### 6.2 Data Governance

**Data Steward**: Platform Engineering team owns the Quota Enforcement schema and notification contracts. The data
steward is responsible for managing the storage plugin contract, maintaining QE abstract projection bases, and
curating the seeded global Quota Resolution Policy.

**Data Classification**: Quota state contains numeric counters, metric names, timestamps, tenant-scoped opaque
identifiers (tenant_id, user_id, subject_id, quota_id, policy_id, idempotency_key, lease_token), operator-supplied
opaque **Quota Metadata** (`cpt-cf-quota-enforcement-fr-quota-metadata`), and caller-supplied opaque \*\*request
metadata \*\* (forwarded into the Engine EvaluationContext per `cpt-cf-quota-enforcement-fr-quota-resolution-engine` and
persisted as part of operation log / idempotency state for replay determinism). All four classes — counters,
identifiers, Quota Metadata, request metadata — are classified as **Platform Operational Data**: internal,
business-sensitive, restricted to authenticated platform actors, no natural-person PII stored directly. Operators are
responsible for Quota Metadata content; **calling services are responsible for sanitizing request metadata before
forwarding it** — the system does not inspect or validate request-metadata values against PII rules, and any PII a
caller injects will land in idempotency records, operation logs, Engine diagnostics, and downstream telemetry per the
request-metadata's natural data flow. Calling services that handle regulated data (GDPR, HIPAA, etc.) MUST strip or hash
such fields before forwarding (e.g., pass `session_id_hash` rather than `session_id`).

**Data Ownership**:

| Data                                        | Owner                            | Custodian                                 |
| ------------------------------------------- | -------------------------------- | ----------------------------------------- |
| Quotas                                      | Tenant identified by `tenant_id` | Quota Enforcement gear (storage plugin) |
| Counters and ledger rows                    | Tenant identified by `tenant_id` | Quota Enforcement gear (storage plugin) |
| Leases                                      | Tenant identified by `tenant_id` | Quota Enforcement gear (storage plugin) |
| Quota Resolution Policies                   | Platform Operator                | Quota Enforcement gear (storage plugin) |
| QE projection bases and owner contracts     | Platform Engineering / metric owner | `types-registry` gear                |
| Idempotency records                         | Tenant identified by `tenant_id` | Quota Enforcement gear (storage plugin) |
| Notification dispatch records (best-effort) | Tenant identified by `tenant_id` | Quota Enforcement gear (storage plugin) |
| Metric (usage type) catalog                 | Platform Engineering             | `types-registry` gear                   |

**Retention**:

| Data class                                    | Retention policy                                                                                                                                                                                                                                     | Reclamation mechanism                                                           |
| --------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Quotas and Quota Resolution Policies (active) | Retained until explicitly deactivated or deleted; the seeded global Policy cannot be deleted                                                                                                                                                         | Operator-initiated only                                                         |
| Quotas (deactivated)                          | Retained indefinitely for read access in P1; no automatic purge. P2 may introduce a configurable grace-period auto-purge tied to audit infrastructure (see §13)                                                                                      | P2 candidate for auto-purge                                                     |
| Counters for consumption-type Quotas          | Active period plus operator-configurable historical window (default: 13 months) sufficient for year-over-year reporting                                                                                                                              | Background sweeper, period-rollover-aware                                       |
| Leases (active and committed/released)        | Active leases live until commit, release, or TTL expiry (lazy expiry per `cpt-cf-quota-enforcement-fr-lease-timeout`). Resolved leases (committed, released, or auto-released) are retained as ledger entries for the operation-log retention window | Background sweeper                                                              |
| Leases (expired but unreclaimed rows)         | Physically reclaimed within an operator-configurable interval after expiry (default: 1 hour) — see `cpt-cf-quota-enforcement-fr-lease-timeout`                                                                                                       | Background sweeper; correctness independent of sweeper liveness (lazy semantic) |
| Idempotency records                           | Operator-configurable per-`(tenant, metric)` retention (default: 24 hours) sufficient to bound legitimate retry windows                                                                                                                              | Background sweeper                                                              |
| Operation log                                 | Operator-configurable (default: 30 days)                                                                                                                                                                                                             | Background sweeper                                                              |
| Notification dispatch records (best-effort)   | Operator-configurable retention for delivery-failure diagnostics (default: 7 days); records older than the window are reclaimed                                                                                                                      | Background sweeper (shared with idempotency cleanup)                            |
| Audit-grade retention                         | Deferred to P2 when platform audit infrastructure is available                                                                                                                                                                                       | P2 component                                                                    |

### 6.3 NFR Exclusions

The following commonly applicable NFR categories are not applicable to this gear:

- **Safety (ISO/IEC 25010:2023 §4.2.9)**: Not applicable — Quota Enforcement is a server-side data API with no physical
  interaction, no safety-critical operations, and no ability to cause harm to people, property, or the environment.
- **Accessibility and Usability (UX)**: Not applicable — Quota Enforcement exposes no user-facing UI. It provides a
  developer SDK and a server-side API consumed exclusively by platform services.
- **Internationalization / Localization**: Not applicable — the gear exposes no user-facing text, labels, or
  locale-sensitive output.
- **Privacy by Design (GDPR Art. 25)**: Not applicable as a standalone gear requirement. Subject IDs stored by Quota
  Enforcement are opaque internal platform identifiers; PII management is the responsibility of the platform identity
  layer (e.g., `account-management`).
- **Regulatory Compliance (GDPR, HIPAA, PCI DSS, SOX)**: Not applicable as a standalone gear requirement — this is an
  internal platform infrastructure gear. Quota Enforcement handles no payment card data (PCI DSS N/A), no healthcare
  records (HIPAA N/A), and no financial reporting data (SOX N/A). Platform-level regulatory obligations are governed at
  the platform level.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### Quota Enforcement SDK Trait

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-interface-sdk-client`

- **Type**: Programmatic SDK (language-native client interface)

- **Stability**: unstable (v0)

- **Description**: Public SDK interfaces cover S2S consumer evaluation and reads, Quota Manager operations, and
  operator-only Policy management. Consumer inputs carry explicit PDP-authorized attribution and operation metadata;
  concrete projection selection remains server-side. Projection registration remains a `types-registry` operation.
  Exact interfaces are defined in DESIGN.md.

- **Breaking Change Policy**: Unstable during initial development; will stabilize in a future version (target: v1).

#### REST API

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-interface-rest-api`

- **Type**: HTTP/REST

- **Stability**: unstable (v0)

- **Description**: REST endpoints covering the same operations as the SDK interfaces, intended for cross-language
  integration and for external consumers that cannot link the Rust SDK directly.

- **Breaking Change Policy**: Unstable during initial development; will follow `/v1/quota-enforcement/...` versioning at
  stabilization.

Projection and constraint contract registration adds **no QE REST endpoint**. Owners publish those schemas through
the existing `types-registry` contract; QE resolves them at Quota/Policy writes and validates request instances at
ingress.

### 7.2 External Integration Contracts

#### Owner-Published Projection Contract

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-contract-owner-projection`

- **Direction**: supplied by the metric owner through `types-registry`; consumed by QE and every Quota Consumer.
- **Protocol/Format**: concrete GTS subject/resource projections, one per-metric request contract, an attached
  constraint contract, and admitted-metric traits. Consumers send `tenant_id`, `{kind,id}` subject refs, and one
  required metadata object; QE derives concrete projection types from the catalogue.
- **Compatibility**: optional-property additions are non-breaking; required additions, removal, rename, retype,
  narrowing, or admitted-metric narrowing require a new contract version.

#### Storage Plugin Contract

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-contract-storage-plugin`

- **Direction**: required from plugin implementor

- **Protocol/Format**: Rust trait implemented by each storage backend plugin.

- **Compatibility**: Plugin contract versioned with the gear's major version; plugins must match the gear's major
  version.

#### Cluster Coordination Contract (consumed)

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-contract-cluster-coordination`

- **Direction**: consumed from the platform `cluster` gear

- **Protocol/Format**: the cluster gear's leader-election primitive, resolved for the `quota-enforcement` cluster
  profile with a linearizable-election requirement. One named election per sweeper singleton (`lease-sweeper`,
  `retention-sweeper`) under the `qe` scope prefix. The operator binds the profile to a backend in the cluster section
  of the deployment YAML; Quota Enforcement code does not change with the backend.

- **Compatibility**: versioned by the cluster gear per primitive; Quota Enforcement tracks the cluster SDK major
  version. A capability mismatch or an unbound profile fails startup (embedded profile) or readiness (deployed
  profile) and never degrades silently. This consumed contract replaces the Quota-Enforcement-owned coordination
  plugin contract that the first version of the coordination ADR defined.

#### Notification Plugin Contract

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-contract-notification-plugin`

- **Direction**: required from notification sink implementor

- **Protocol/Format**: Rust trait implemented by each notification sink plugin in P1; P2 will additionally route through
  the platform EventBus.

- **Compatibility**: Plugin contract versioned with the gear's major version; backwards-compatible additive changes
  are allowed within a major version.

#### Subject Manager Lifecycle Contract — Indirect via Quota Manager

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-contract-subject-manager`

- **Direction**: indirect — Subject Managers (e.g., `account-management`) signal subject lifecycle events to Quota
  Manager (a separate platform component); Quota Manager consumes those events upstream and reaches Quota Enforcement
  only through QM-issued CRUD on Quota records (see `cpt-cf-quota-enforcement-fr-quota-lifecycle`). Quota Enforcement
  defines no direct Subject Manager-facing API surface in P1; this entry exists to document the indirection rather than
  a QE-side endpoint.

- **Protocol/Format**: not a QE-owned protocol — QE observes Subject Manager intent solely through Quota Manager-driven
  Quota CRUD. A direct contract (REST or gRPC) MAY be added in a later major release if a use case justifies bypassing
  Quota Manager; it would land here as additive without breaking P1 callers.

- **Compatibility**: P1 exposes no Subject Manager-facing surface to version. Future direct contract would follow
  backwards-compatible additive changes within a major version.

#### Quota Resolution Engine Plugin Contract

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-contract-quota-resolution-engine-plugin`

- **Direction**: required from Engine implementor; P1 built-ins (`most-restrictive-wins`, `cel`) ship with the gear.
  Additional Engines are linked into the gear binary at build time.

- **Protocol/Format**: Rust trait (`QuotaResolutionEngineV1`) with config-validation and evaluate entry points;
  `engine_config` is opaque to the Quota Enforcement core and validated by the Engine's own validator. Decision shape is
  `{ result, debit_plan: Map<quota_id, QuotaDebitPlan>, diagnostics }` with the Debit-Plan invariants enforced at the QE
  core boundary (see `cpt-cf-quota-enforcement-fr-quota-resolution-engine`).

- **Compatibility**: Plugin contract versioned with the gear's major version; backwards-compatible additive changes (
  new optional `QuotaDebitPlan` fields, new `EvaluationContext` bindings) are allowed within a major version. Removing
  or changing the meaning of an existing field is a major-version break.

## 8. Use Cases

#### Create a Quota

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-usecase-create-quota`

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator` or `cpt-cf-quota-enforcement-actor-quota-manager` (acting
on behalf of a tenant administrator)

**Preconditions**:

- Caller is authenticated with the appropriate scope (operator-level for cross-tenant operations; Quota Manager
  forwarding a tenant administrator's SecurityContext for own-tenant operations)
- Target metric is registered in `types-registry`
- Target owner projection is registered, concrete, configured for evaluation, derived from the QE subject base, and
  admits the metric

**Main Flow**:

1. Caller submits a Quota payload (subject reference, metric, type, period if applicable, `enforcement_mode`, cap,
   `source` (defaults to `licensing` when omitted; `operator` for manual caps), optional notification thresholds,
   optional validity window, optional failure-mode hint, optional metadata)
1. System validates the payload (metric exists; type/period combinatorics are valid; owner projection is registered,
   concrete, present in the configured evaluation catalogue, derived from the QE subject base, and admits the metric;
   Quota metadata conforms to its separate owner contract).
1. System persists the Quota with a server-assigned quota ID and emits a `quota-changed` notification event

**Postconditions**:

- The Quota is immediately effective and joins any other applicable Quotas; arbitration is resolved by the active Quota
  Resolution Policy (default `most-restrictive-wins`).

**Alternative Flows**:

- **Unknown metric**: System rejects with actionable error
- **Projection outside the configured catalogue**: System rejects with `PROJECTION_NOT_RESOLVABLE`
- **`types-registry` unreachable**: System rejects with actionable error
- **`type=rate`**: System rejects with `NOT_YET_IMPLEMENTED` error in P1
- **Invalid projection or constraint contract**: System rejects with the corresponding canonical field/precondition error

#### Configure a Quota Resolution Policy

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-usecase-configure-policy`

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator`

**Preconditions**:

- Operator is authenticated with operator-level SecurityContext
- The default global Policy (most-restrictive-wins) is already active

**Main Flow**:

1. Operator submits a Policy payload (scope: `metric=<name>`; `engine_id`: e.g., `cel`; `engine_config`: engine-specific
   structure such as a CEL expression; optional per-Policy timeout)
1. System resolves `engine_id` against the registered Engines (rejects unknown engine_id with actionable error) and
   delegates `engine_config` validation to the named Engine's validator (e.g., the `cel` Engine performs CEL parse +
   type-check; parse/type errors are returned with line/column)
1. System persists the Policy with a server-assigned policy ID and emits a `policy-changed` event
1. Subsequent evaluations matching the Policy's scope use the new Policy via most-specific-scope precedence

**Postconditions**:

- The Policy is immediately effective for matching evaluations
- The default global Policy remains in place for non-matching evaluations

**Alternative Flows**:

- **Unknown `engine_id`**: System rejects with `UNKNOWN_ENGINE` error naming the registered Engines available in this
  deployment
- **Engine config validation error** (e.g., CEL parse error): System rejects with actionable error from the Engine's
  validator (e.g., line/column for CEL)

#### Author a Quota Cascade Policy via CEL

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-usecase-cascade-via-cel`

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator`

**Preconditions**:

- A tenant `T` has both `(owner-user-projection, U)` and `(owner-tenant-projection, T)` Quotas for metric `M`
  (e.g., `ai-tokens-input`); operator wants
  per-user pool consumed first, falling through to the tenant pool when user-pool is exhausted
- The `cel` Engine is registered in this deployment

**Main Flow**:

1. Operator submits a Policy payload with `scope=metric=M`, `engine_id=cel`, and `engine_config.expr` containing a CEL
   expression that emits a sparse `debit_plan` based on per-pool remaining capacity (route to the owner's user-scope
   Quota first; fall through to its tenant-scope Quota for the residual; deny when both pools combined are insufficient — see
   `cpt-cf-quota-enforcement-fr-quota-cascade` for the worked example)
1. System validates the CEL expression and persists the Policy
1. A subsequent debit `debit(metric=M, amount=A, …)` for a user `U` in tenant `T` resolves applicable-Quotas →
   `{user_q, tenant_q}` → invokes the `cel` Engine, which emits a Debit Plan that draws from `user_q` first
1. Quota Enforcement validates the Debit Plan against the standard invariants and atomically applies it; counter
   mutations are limited to the Quotas named in the plan

**Postconditions**:

- Operations split across `user_q` and `tenant_q` as authored by the `cel` expression: `user_q` contributes up to its
  remaining capacity, `tenant_q` covers the residual; deny when `user_q.remaining + tenant_q.remaining < amount`
- The **multi-entry split** is the defining capability over the default `most-restrictive-wins` Engine. When
  `user_q.remaining > 0` but `< amount`, this `cel` Policy emits a two-entry plan
  (`{user_q: user_q.remaining, tenant_q: amount − user_q.remaining}`); `most-restrictive-wins` would instead fall
  through to a single-entry plan on `tenant_q` (consuming the full `amount`) and leave `user_q.remaining` unused.
  When `user_q.remaining = 0`, both Engines route everything to `tenant_q` and produce the same plan

**Alternative Flows**:

- **Both pools insufficient**: Engine returns `Denied(violated_quota_ids=[user_q.id, tenant_q.id])`; counters are not
  modified
- **Engine produces malformed plan** (e.g., references an unknown quota_id, an entry's `amount` exceeds
  `request.amount`, negative `amount`): system enforces Debit-Plan invariants and surfaces a canonical `Internal`
  error (fail-closed) with no counter mutation; the operator is alerted via telemetry

#### Author a Region-Gated Quota via Metadata

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-usecase-region-gated-via-metadata`

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator`

**Preconditions**:

- Tenant T has user U; metric `gpu-hours` is registered in `types-registry`
- Operator wants per-region caps for U: 100 in `us-east-1`, 50 in `eu-west-1`
- Calling services pass `region` in the operation-level `metadata` on every `debit` invocation
- An Engine that produces sparse Debit Plans based on attribute matching is registered (P1: `cel`)

**Main Flow**:

1. Operator creates two Quotas via `cpt-cf-quota-enforcement-usecase-create-quota`:
   - `Q_us`: `subject=(owner-user-projection, U)`, `metric=gpu-hours`, cap=100, hard,
     `metadata={regions: ["us-east-1"]}`
   - `Q_eu`: `subject=(owner-user-projection, U)`, `metric=gpu-hours`, cap=50, hard,
     `metadata={regions: ["eu-west-1"]}`
1. Operator creates a Quota Resolution Policy with `scope=metric=gpu-hours`, `engine_id=cel`, and `engine_config.expr`
   containing an expression that filters `applicable_quotas` by `request.region in arbitration.regions` and
   emits a Debit Plan over the matching Quota
1. Calling service makes the request `debit(metric=gpu-hours, amount=20, metadata={region: "us-east-1"})`
1. Quota Enforcement resolves `applicable_quotas = {Q_us, Q_eu}`, invokes the Engine, applies Debit Plan
   `{ Q_us.id: { amount: 20 } }`
1. `Q_us.consumed` increases by 20; `Q_eu.consumed` is **not modified**

**Postconditions**:

- The user's regional usage is tracked independently per-region; the per-region cap is enforced
- A subsequent request with `region=eu-west-1` is debited against `Q_eu` per the same Policy

**Alternative Flows**:

- **Request metadata `region` matches no Quota** (e.g., `region=ap-south-1`): Policy returns
  `Denied(violated_quota_ids=[], reason="no Quota matches request region")`, or routes to a fallback Quota the operator
  declared with sentinel metadata (`metadata={regions: ["*"]}`)
- **Metadata exceeds 4 KB byte-size limit** at create or update (per `cpt-cf-quota-enforcement-fr-quota-metadata`):
  rejected with an actionable validation error

#### Debit a Quota for an Operation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-usecase-debit`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Preconditions**:

- Quota Consumer is authenticated as a service principal and PDP authorizes its supplied tenant/subject/metric tuple
- At least one Quota is active for one or more applicable subjects and the requested metric. If catalogue mapping
  produces zero applicable Quotas for a `QuotaGated` metric, Quota Enforcement returns
  `Denied(violated_quota_ids=[], reason="NO_APPLICABLE_QUOTA")` per §3.2 — operators must provision Quotas (typically
  via subject-manager hooks at tenant/user creation) before consumption is permitted.

**Main Flow**:

1. Consumer calls `debit(tenant_id, subjects, metric, amount, metadata, resource?, idempotency_key)`
1. QE rejects malformed public request shape; PDP authorizes the complete structurally valid tuple
1. QE maps each authorized `(metric, kind)` to an owner projection and validates request/resource contracts
1. Quota Enforcement fetches all applicable Quotas for the operation's metric
1. Quota Enforcement selects the active Quota Resolution Policy by most-specific-scope precedence, then invokes that
   Policy's Engine (e.g., `most-restrictive-wins` or `cel`) against the applicable-Quotas set; the Engine returns a
   Decision with `result`, `debit_plan`, and `diagnostics`
1. Quota Enforcement validates the Decision against the Debit-Plan invariants (
   `cpt-cf-quota-enforcement-fr-quota-resolution-engine`) and, on `Allowed`, atomically applies the `debit_plan`: each
   Quota named in the plan is mutated by `entry.amount`
1. Quota Enforcement returns the Decision to the caller; consumer applies it (e.g., proceeds with the guarded operation
   on `Allowed`, returns 429 on `Denied`)

**Postconditions**:

- Counter state reflects the `debit_plan` on `Allowed` (Quotas NOT named in `debit_plan` are not touched, even if they
  appeared in the applicable-Quotas set)
- Counter state is unchanged for `Denied` outcomes and for any canonical-error response
- `threshold-crossed` notification events are emitted for each Quota whose post-mutation usage crossed an upward
  threshold configured in `notification_thresholds`

**Alternative Flows**:

- **Replay (same idempotency key, same payload)**: returns the original decision; counters are not modified again
- **Replay (same idempotency key, divergent payload)**: rejected with `IDEMPOTENCY_PAYLOAD_MISMATCH`
- **Engine returns malformed `debit_plan`** (unknown quota_id, an entry's `amount` exceeds `request.amount`, negative
  amount): system rejects with a canonical `Internal` error (fail-closed) and emits engine-invariant-violation
  telemetry
- **Engine timeout (per-Policy default 5ms)**: surfaces a canonical `DeadlineExceeded` error (fail-closed); no counters
  mutated
- **PDP unreachable mid-evaluate**: surfaces a canonical `ServiceUnavailable` error (fail-closed)

#### Reserve, Then Commit (or Release) for a Long-Running Operation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-usecase-reserve-and-commit`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Preconditions**:

- Same as Debit use case; a worst-case capacity estimate is available for lease

**Main Flow**:

1. Consumer calls `reserve(metric, amount, ttl, idempotency_key)` with the worst-case estimate; `ttl` **MUST** lie
   within the operator-configurable `[min_lease_ttl, max_lease_ttl]` window per `cpt-cf-quota-enforcement-fr-lease-acquire`
1. Quota Enforcement evaluates the lease against applicable Quotas under the active Quota Resolution Policy; returns
   `lease_token` on success or denial otherwise
1. Consumer performs the long-running operation (e.g., 30-minute compute job)
1. Consumer calls `commit(lease_token, actual_amount, idempotency_key)` with the realized usage
1. Quota Enforcement converts the lease into a debit of `actual_amount`; the difference (reserved minus actual) is
   returned to each affected Quota's remaining capacity

**Postconditions**:

- Counter state reflects the actual consumed amount
- The lease is marked as `committed`

**Alternative Flows**:

- **Operation aborted**: Consumer calls `release(lease_token, idempotency_key)`; held capacity is fully returned
- **Lease TTL expired before commit**: System auto-releases the lease, emits `lease-auto-released`; subsequent `commit`
  returns `LEASE_NOT_ACTIVE`
- **Actual amount exceeds reserved**: rejected with `OVER_COMMIT_NOT_AUTHORIZED`

#### Product Backend Serves an End-User Quota Snapshot Request

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-usecase-end-user-quota-snapshot-read`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Preconditions**:

- An end user is authenticated by the consuming product
- The product backend has a service principal authorized to read the target tenant/user quota state

**Main Flow**:

1. End user requests their personal quota state through the product
1. The product backend calls QE's S2S snapshot endpoint with explicit `tenant_id` and user `{kind,id}`
1. QE checks the public target shape; PDP authorizes the complete structurally valid target against the backend service principal
1. QE maps the authorized tenant/user kinds through the catalogue
1. Quota Enforcement fetches all active Quotas for these subjects
1. Quota Enforcement returns per-Quota state for every applicable authorized Quota per
   `cpt-cf-quota-enforcement-fr-end-user-quota-snapshot-read` (state contract identical to the base
   `cpt-cf-quota-enforcement-fr-quota-snapshot-read`; no Policy attribution; exact field shape is a DESIGN concern)
1. The product backend renders the response for the end user

**Postconditions**:

- End user receives a complete view of their applicable Quotas through the product, without administrator intervention

**Alternative Flows**:

- **No applicable Quotas**: Quota Enforcement returns an empty list (not an error); the product surfaces "no quotas
  apply"
- **Backend supplies an unauthorized tenant or user**: PDP returns `PermissionDenied`; QE performs no storage read.

#### Bulk Debit Across Multiple Quotas

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-usecase-batch-debit`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Preconditions**:

- Caller has multiple debit items to apply for a single logical operation that requires all metrics to be admitted
  together (e.g., LLM call needing input tokens, output tokens, and compute seconds)

**Main Flow** (`mode = atomic`, P1):

1. Consumer calls `batch_debit(items[], envelope_idempotency_key, mode = "atomic")`
1. Quota Enforcement evaluates each item against its applicable-Quotas set under the active Quota Resolution Policy;
   each item's evaluation observes counter state reflecting every previously-evaluated item in the same batch
1. If every item evaluates to `Allowed`, Quota Enforcement applies the union of all per-item `debit_plan`s and returns a
   batch-level `Allowed` outcome with a per-item array preserving submission order
1. If any item evaluates to `Denied` or fails with a canonical error, Quota Enforcement applies no counter mutations and
   returns the batch-level outcome (`Denied` in the body, or the canonical error envelope) with per-item statuses for
   diagnostics

**Postconditions**:

- Atomic batch: either all counters in the union of `debit_plan`s are mutated, or none are
- Replay with the same envelope key returns the original outcome

**Alternative Flows**:

- **Batch exceeds maximum size**: rejected with `BULK_TOO_LARGE` error before any item is evaluated
- **`mode = independent`**: rejected with `NOT_YET_IMPLEMENTED` in P1; deferred to P2 for bulk-independent operations
- **Batch-level timeout (atomic)**: the entire batch surfaces a canonical `DeadlineExceeded` error
  (`reason = "BATCH_TIMEOUT"`) with no counter mutations; caller retries with the same envelope key or a smaller batch

## 9. Acceptance Criteria

- [ ] Quotas can be created, updated, deactivated, and read; deactivated Quotas stop accepting new debits but remain
  readable
- [ ] Quota creation accepts multiple Quotas for the same `(subject, metric)` pair; all such Quotas enter the
  applicable-Quotas set at evaluation time and are resolved by multi-quota evaluation under the active Policy
- [ ] QE registers abstract subject/resource/request/constraint bases; owner projections replace platform-wide seeded
  tenant/user subject types; bootstrap verifies `(metric, scope)` uniqueness, admitted metrics, exactly one request
  contract per metric, and its attached constraint contract
- [ ] A consumer operation supplying `tenant_id=T` and subject `{kind=user,id=U}` maps both tenant and user scopes to
  the metric owner's projections; tenant-scoped Quotas constrain every user in the tenant and remain shared across callers
- [ ] PDP authorizes the complete caller-supplied tenant/subject/metric/resource tuple against the authenticated service
  principal before catalogue mapping or evaluation
- [ ] Ingress error precedence is stable: malformed public request shape returns `InvalidArgument` before PDP;
  structurally valid unauthorized attribution returns `PermissionDenied` before catalogue or contract lookup; authorized
  unknown/unadmitted kinds and invalid contract payloads return `InvalidArgument`
- [ ] Every subject-based consumer evaluation request sends required operation-level `metadata` and only registered, admitted scope
  kinds; violations return canonical `InvalidArgument`, never `Decision::Denied`
- [ ] Two different services debiting one owner metric use the same owner projection and resolve the same applicable-
  Quota key; a caller-specific projection is rejected by contract governance
- [ ] Attribution and authenticated principal data are excluded from Policy input; PDP and `token_scopes` remain the
  permission boundary
- [ ] Allocation Quotas track in-flight capacity without periodic reset; consumption Quotas track per-period consumption
  with reset at calendar-aligned UTC boundaries
- [ ] `type=rate` Quota creation requests are rejected with `NOT_YET_IMPLEMENTED` in P1 per
  `cpt-cf-quota-enforcement-fr-quota-type-rate-rejection`
- [ ] Period rollover resets the consumed counter to zero atomically with respect to in-flight operations and emits a
  `period-rollover` notification event with closing-period consumed amount, closing-period cap, and new period boundary
- [ ] `period-rollover` event for `P` MAY be delayed up to `max_lease_ttl` past the calendar boundary (default 1 hour)
  because P-attributed leases may settle cross-period; the event signals **settlement completion**, not the calendar
  transition itself; consumers needing a calendar-time signal (dashboards, threshold-marker resets, utilization-roll-up
  alerts) compute it from `period_boundary` arithmetic without subscribing to any event
- [ ] Credit closure is calendar-keyed: `credit` to period `P` rejected with `PERIOD_CLOSED` from `time >= period_end`
  onwards, regardless of whether `period-rollover` has been emitted for `P` yet
- [ ] Rollback closure is settlement-keyed: rollback to period `P` rejected with `PERIOD_CLOSED` once the
  `period-rollover` event has been emitted for `P`; the settlement window between `period_end` and event emission keeps
  cross-period lease commits reversible
- [ ] Debit, credit, rollback, reserve, commit, and release are idempotent under retry; replay with same key + same
  payload returns the original outcome; replay with divergent payload is rejected
- [ ] Credit takes an explicit `quota_id` and mutates only the named Quota's counter (no subject resolution, no Engine
  invocation); cross-tenant `quota_id`, unknown `quota_id`, or `quota_id` of a deactivated Quota are rejected with
  actionable error before any mutation; every successful credit emits one `quota-counter-adjusted` event with the
  credited amount, target `quota_id`, and authenticated service principal
- [ ] Lease auto-releases at TTL expiry; subsequent commit/release returns `LEASE_NOT_ACTIVE`; auto-release emits
  `lease-auto-released`
- [ ] Lease commit with `actual_amount ≤ reserved_amount` is allowed; `actual_amount > reserved_amount` is rejected with
  `OVER_COMMIT_NOT_AUTHORIZED`
- [ ] The seeded global Quota Resolution Policy uses `engine_id=most-restrictive-wins` with empty config; cannot be
  deleted but may be replaced; under it, multi-quota evaluation denies on any Quota violation identifying every
  violating Quota; P1 admission outcomes are `Allowed` / `Denied` (Decision body) or a platform-canonical error
- [ ] Quota Resolution Policies can be created with scope `metric` (in addition to the seeded `global` Policy);
  most-specific-scope precedence (`metric` > `global`) selects the active Policy at evaluation time; Policies
  referencing an unknown `engine_id` are rejected at create/update time; narrower scopes than `metric` are not reachable
  through the P1 API and are tracked as an Open Question
- [ ] Quota Resolution Engine plugin contract is implemented; P1 ships `most-restrictive-wins` (hardcoded) and `cel`
  (sandboxed); both are registered automatically at gear bootstrap
- [ ] Engine config validation is delegated to the named Engine at Policy create/update; the `cel` Engine performs parse
  \+ type-check at this stage and rejects syntactic and type errors with line/column; the `most-restrictive-wins` Engine
  rejects any non-empty config
- [ ] Quota Resolution Policy versioning: `create_policy` initializes `policy_version = 1` with
  `version_state = active`; subsequent `update_policy(if_match_version=N)` calls produce monotonically increasing
  versions, transition the previous active version to `superseded`, and atomically move the latest-pointer;
  `update_policy` with mismatched `if_match_version` is rejected with `VERSION_CONFLICT`;
  `rollback_policy(target_version)` makes the target active and transitions the previous active to `rolled_back`
  (terminal); rollback to a `rolled_back` or non-existent version is rejected with actionable error;
  `delete_policy(policy_id)` soft-deletes the Policy — active version transitions to `deleted` (terminal),
  latest-pointer cleared, evaluation falls through to the next-most-specific scope, `policy-changed` event emitted with
  `change_kind = "deleted"`; `delete_policy` against the seeded global Policy is rejected with
  `CANNOT_DELETE_SEEDED_GLOBAL_POLICY`; the seeded built-in global Policy materializes at bootstrap as
  `policy_version = 1`
- [ ] P1 rejects breaking projection-version activation: Quota and Policy writes referencing a projection outside the
  configured catalogue fail, and bootstrap fails when the catalogue is incompatible with an active Quota or Policy
- [ ] Consumer DTOs contain no `caller_type`; the authenticated service principal is taken from `SecurityContext`, and
  Policy input is limited to `{request, resource, arbitration}`
- [ ] Decision diagnostics on every evaluation include `engine_id`, `policy_id`, and `policy_version` of the Policy that
  produced the Decision; idempotency replay returns the Decision with the original `engine_id` and `policy_version`
  verbatim, even after the Policy has been updated to a higher version
- [ ] `superseded`, `rolled_back`, and `deleted` Policy versions are retained for the operator-configured retention
  window (default 90 days), then hard-deleted by the storage retention sweeper; `active` versions are never auto-deleted
- [ ] Decision shape carries `result ∈ {Allowed, Denied}`, `debit_plan: Map<quota_id, QuotaDebitPlan{amount}>`, and
  `diagnostics`; counters are mutated strictly per `debit_plan` when result is `Allowed`, and not mutated for `Denied`
  or for any canonical-error response
- [ ] Debit-Plan invariants are enforced at the Engine boundary: quota_ids ⊆ applicable-Quotas set; per-entry
  `0 ≤ amount ≤ request.amount` (integer); result/plan consistency (`Allowed` ⇒ non-empty plan;
  `Denied` ⇒ empty plan). Any violation surfaces a canonical `Internal` error with no counter mutation; the
  `debit_plan_invariant_violations_total` counter records: `quota_id_outside_applicable_set`, `negative_amount`,
  `amount_exceeds_request_amount`, `result_plan_inconsistency`
- [ ] `debit`, `credit`, `reserve`, and per-item `batch_debit` requests with `amount ≤ 0` are rejected with an
  actionable `INVALID_AMOUNT` error **before** idempotency lookup or any other pipeline step; no idempotency record is
  persisted, no operation log entry is written, no counter / lease row / hold is created. For `batch_debit`, the
  envelope is rejected naming the offending item index when any item carries `amount ≤ 0`
- [ ] `most-restrictive-wins` Engine produces a Debit Plan with a **single entry** — the binding Quota — at
  `amount = request.amount`. Binding-Quota selection ranks satisfiable Quotas (remaining ≥ `request.amount`; unbounded
  trivially satisfiable) by three priority rules in order: (1) most-specific subject-scope tier wins (P1: user-scope >
  tenant-scope); (2) within the chosen tier, bounded > unbounded (operator's explicit cap takes precedence; unbounded
  acts as overflow when no bounded satisfiable in the tier); (3) within bounded satisfiable Quotas of the chosen tier,
  smallest remaining wins, ties broken by ascending `quota_id`. `Denied` is returned only when no Quota is satisfiable
  (every applicable bounded Quota has remaining capacity below `request.amount` and no unbounded Quota exists);
  `violated_quota_ids` then enumerates every such bounded Quota
- [ ] Cascade scenario (`cpt-cf-quota-enforcement-fr-quota-cascade`) verified end-to-end: with user-pool empty and
  tenant-pool available, an operator-authored Policy that routes user→tenant produces `Allowed` with `debit_plan`
  covering only `tenant_q`; user counter is not mutated; with both pools insufficient, `Denied` is returned naming both
  Quotas; with malformed cascade plan, system surfaces a canonical `Internal` error
- [ ] Quota Metadata (`cpt-cf-quota-enforcement-fr-quota-metadata`) is validated at create/update against the metric
  constraint contract attached to the metric request contract and the 4 KB bound; stored metadata is not revalidated on evaluation
- [ ] Quota Metadata is exposed verbatim to CEL as `arbitration`; Engines see it alongside the operation-level
  `request` object and optional `resource`; QE core does not interpret metadata keys or values
- [ ] Metadata changes emit `quota-changed` events without invalidating the quota_id; Policies referencing the new
  metadata values take effect on the next evaluation
- [ ] Region-gated scenario (`cpt-cf-quota-enforcement-fr-attribute-based-quota-selection`) verified end-to-end: with
  `Q_us(metadata.regions=[us-east-1])` and `Q_eu(metadata.regions=[eu-west-1])`, a `debit` carrying
  `metadata.region=us-east-1` debits only `Q_us`; `Q_eu` is not mutated; a request with `region=ap-south-1`
  produces `Denied` per the operator's authored fallback; a missing required `region` is rejected at request ingress
- [ ] CEL Policy create/update statically validates property references and request↔arbitration pair compatibility,
  including type/domain/cardinality mismatch, with line/column diagnostics
- [ ] Decision is response-only: request DTOs do not carry Decision-shaped fields by type design; any Decision-shaped
  field (`debit_plan`, `result`, `diagnostics`) appearing in a request body is silently ignored by the server,
  regardless of operation kind (debit, credit, rollback, reserve, commit, release, batch_debit)
- [ ] Idempotency replay returns the original Decision (including `debit_plan`); client-submitted Decision-shaped fields
  in a replay payload are silently ignored per the trust-boundary discipline (§3.4)
- [ ] Lease rows are physically reclaimed by the sweeper within the operator-configured interval after expiry ( default:
  ≤ 1 hour); telemetry surfaces unreclaimed-expired-lease count by canonical registered `metric`; sweeper outage does
  not affect correctness (lazy expiry semantic)
- [ ] Per-`(tenant, metric)` active-lease cap (default: 1000) is enforced at acquisition time; requests exceeding the
  cap are rejected with `LEASE_INFLIGHT_LIMIT_EXCEEDED`; expired leases do not count toward the cap
- [ ] Lease TTL is required on every acquire request and **MUST** fall within the operator-configurable
  `[min_lease_ttl, max_lease_ttl]` window (platform defaults: 1 s / 1 h); a missing `ttl` or a value outside the window is rejected
  with `TTL_OUT_OF_BOUNDS` before idempotency lookup, multi-quota evaluation, or any hold acquisition — no idempotency
  record, no lease row, no capacity hold, and no change to the per-`(tenant, metric)` active-lease counter; clamping is
  not performed
- [ ] Multi-Quota lease acquisition is atomic: when the Engine's Debit Plan names multiple Quotas, holds are placed on
  every named Quota or none; partial holds are never observable, including under concurrent contention. Concurrent
  multi-Quota lease traffic does not deadlock the acquisition path
- [ ] Acquisition contention timeout (operator-configurable, default: 0 ms / fail-fast) bounds wait time on row
  contention; exceeded timeouts produce `LEASE_CONTENTION_TIMEOUT` rejection with no holds;
  `lease_contention_rejected_total` and `lease_acquisition_wait_seconds` metrics are populated by canonical registered
  `metric` and let operators distinguish and localize contention-driven denials from cap-exceeded, not-active, and
  Engine-Denied rejections
- [ ] Quota deactivation while leases are outstanding: active leases are marked `resolved-by-deactivation` atomically
  with the deactivation transaction; subsequent `commit` / `release` against those leases return `LEASE_NOT_ACTIVE`; a
  `lease-resolved-by-deactivation` notification event is emitted per affected lease
- [ ] Cap reduction below current consumed amount is rejected with `CAP_BELOW_CONSUMED`; the comparison is evaluated at
  update transaction commit time (not request-receipt) to avoid TOCTOU races with concurrent debits; cap raises and
  equal-or-greater updates are unaffected
- [ ] Period rollover ordering: a debit committed at exactly `boundary_at` is accounted to period `P+1`;
  `period-rollover` event for period `P` is emitted strictly after the last commit attributed to `P`
- [ ] Idempotency replay returns the original Decision verbatim from server-side state; the original `time` binding is
  captured but NOT re-bound on replay (verified end-to-end with a time-dependent CEL Policy: original evaluation at 02:
  30 returns Denied; replay at 04:00 still returns Denied because the stored Decision is the source of truth)
- [ ] Batch-debit batch-level evaluation timeout supersedes per-Policy timeouts; the timeout is a single
  operator-configurable flat duration (deployment-default 250 ms) per `cpt-cf-quota-enforcement-fr-batch-debit` (§5.7);
  on timeout, atomic-mode batch surfaces a canonical `DeadlineExceeded` error (`reason = "BATCH_TIMEOUT"`) with no
  counter mutations; in independent mode (P2), pending and not-yet-completed items are reported with `BATCH_TIMEOUT`
  while already-completed items retain their `Decision` outcomes
- [ ] Atomic batch-debit per-item evaluation observes running batch state: each item's Engine call sees counter state
  reflecting every previously-evaluated item in the same batch. Verified end-to-end — `Quota Q (cap=800, consumed=0)`;
  `batch_debit(mode=atomic, items=[{metric=M, amount=500}, {metric=M, amount=500}])` returns batch-level `Denied`
  (item-1 evaluated against `remaining=800` returns `Allowed`; item-2 evaluates against `remaining=300` and returns
  `Denied`; batch rolls back, no mutations persisted)
- [ ] Engine bootstrap is fail-fast: if any built-in Engine in the deployment manifest fails to register, the gear
  fails readiness, refuses requests, increments `engine_bootstrap_failures_total`, and emits a structured log entry; the
  gear does NOT silently fall back to a different Engine
- [ ] Validity-window semantics are Engine-driven: built-in `most-restrictive-wins` excludes Quotas whose `time` falls
  outside `[validity_start, validity_end]` from the Debit Plan by default; operator-authored Policies MAY override (
  grace periods, expected-window matching against `request.expected_window`); leases acquired within a valid
  window remain commit-able after `validity_end`; `currently_within_window` boolean is surfaced on every Quota Snapshot
  read
- [ ] Cross-period-boundary lease attribution (consumption Quotas): a lease is attributed to the period containing its
  acquisition timestamp; commit/release/auto-release that fire after a period rollover apply against the acquisition
  period's counter, not the new period's. Verified end-to-end — `Quota Q (period=day, cap=1000, consumed=0)` at 23:59
  UTC: `reserve(amount=200)` succeeds, `Q.consumed_today += 200`. After 00:00 UTC rollover,
  `Q.consumed_today (new period) = 0`; `commit(lease_token, actual=150)` applies to the **prior** day's counter (closed)
  and does not increment the new day's `consumed`
- [ ] Rollback targeting a debit attributed to a settled period (consumption Quotas; settlement defined as
  `period-rollover` event emitted for the attribution period) is rejected with `PERIOD_CLOSED` per
  `cpt-cf-quota-enforcement-fr-rollback`; rollbacks within the active period and within the settlement window (between
  `period_end` and `period-rollover` event) continue to succeed normally; for cross-period guaranteed cancellation,
  callers use the lease flow
- [ ] Rollback identifies a debit by the idempotency key of the call that produced it, regardless of whether the debit
  originated from a direct `debit` call or a lease `commit`. Verified end-to-end — `reserve(amount=200, key=K1)` →
  `commit(actual=150, key=K2)` produces a debit registered under `K2`; `rollback(key=K2)` restores `Q.consumed` by 150.
  Rollback of an unsettled lease is **not** supported via this primitive — callers use `release`
  (`cpt-cf-quota-enforcement-fr-lease-release`) before commit
- [ ] Every successful rollback emits exactly one `quota-rollback-applied` notification event carrying the original
  debit's idempotency key, the rolled-back amount, the target Quota, and the consumer identity; the event kind is
  distinct from `quota-counter-adjusted` (which fires only for credits per `cpt-cf-quota-enforcement-fr-credit`).
  Rollbacks rejected with `PERIOD_CLOSED` or `UNKNOWN_OPERATION` emit no event
- [ ] Negative `cap` is rejected at create/update with `CAP_MUST_BE_NON_NEGATIVE`; `cap = 0` is permitted with
  documented semantics (`hard` Quota with `cap=0` denies every debit); `cap = null` (unbounded) is permitted with
  documented semantics (always satisfiable, counter still increments, `remaining` reported as `null`);
  `notification_thresholds` on unbounded Quotas are rejected at create/update with `THRESHOLDS_REQUIRE_BOUNDED_CAP`;
  the `quota_cap_zero_total` and `quota_cap_unbounded_total` gauges count the active Quotas of each kind
- [ ] Quota record carries `enforcement_mode` from GTS instances under `gts.cf.qe.enforcement.type.v1~` (P1: only
  `hard` is accepted); attempts to create a Quota with an unsupported `enforcement_mode` value are rejected with an
  actionable error; future values are added as new GTS instances per `cpt-cf-quota-enforcement-fr-enforcement-mode`
- [ ] Quota record carries `source` from GTS instances under `gts.cf.qe.source.type.v1~` (P1: `licensing` (default),
  `operator`); attempts to create a Quota with an unsupported `source` value are rejected with an actionable error;
  mutation rules in P1 are uniform across both source values (operator-level or Quota-Manager PDP scope); P2
  source-kind extensions (`tenant_admin`, `user_self`) and per-source mutation rules are additive per
  `cpt-cf-quota-enforcement-fr-quota-lifecycle`
- [ ] (P2) Bulk Quota CRUD endpoints (`cpt-cf-quota-enforcement-fr-bulk-quota-crud`) — `bulk_create_quotas`,
  `bulk_update_quotas`, `bulk_deactivate_quotas` — are transactional: partial failure rolls back the entire batch;
  envelope idempotency replay returns original outcome; max batch size (default 50, operator-configurable) is enforced
  with `BULK_TOO_LARGE`; same PDP, tenant-isolation, and trust-boundary rules apply as for single-item CRUD
- [ ] Threshold notifications fire once per upward transition: a `Denied` outcome or a canonical-error response does NOT
  emit `threshold-crossed` (counters not modified, no transition); an `Allowed` mutation crossing one or more thresholds
  emits exactly **one** `threshold-crossed` event whose payload carries `crossed_thresholds` (ascending list) and
  `highest_crossed_threshold`. The highest-crossed marker per `(Quota, period)` resets at period rollover. Verified:
  `consumed` 30 → 85 with `notification_thresholds = [50, 80, 100]` emits 1 event (`crossed=[50,80]`, `highest=80`);
  subsequent `consumed` 85 → 90 emits 0 events; period rollover resets, new-period `consumed` 0 → 60 emits 1 event
  (`crossed=[50]`, `highest=50`)
- [ ] Idempotency-key uniqueness is scoped per `(tenant_id, idempotency_subject_key, operation_type, key)` with consumer attribution
  accepted only after PDP authorization; the same key string reused across any of the four scope dimensions — different
  `tenant_id`, different `idempotency_subject_key`, or different `operation_type` — creates independent idempotency records and is never
  cross-matched; payload divergence within the **same** `(tenant_id, idempotency_subject_key, operation_type, key)` scope is rejected
  with `IDEMPOTENCY_PAYLOAD_MISMATCH` per `cpt-cf-quota-enforcement-fr-idempotency`
- [ ] Telemetry counters for lease-inflight-limit and Debit-Plan-invariant rejection paths are exposed:
  `lease_inflight_limit_exceeded_total`, `debit_plan_invariant_violations_total` (with documented invariant values);
  each counter increments on its corresponding rejection path and is queryable via the standard metrics scrape;
  `lease_inflight_limit_exceeded_total` is labelled by canonical registered `metric`
- [ ] Operations targeting a Quota whose metric is classified `Direct` in `types-registry` are rejected with
  `METRIC_NOT_QUOTA_GATED` for every write/preview entry-point (`debit`, `credit`, `rollback`, `reserve`, `commit`,
  `release`, batch-debit, `evaluate-preview`) — counters are not mutated, no idempotency record / operation log entry /
  lease row is created; Quota Snapshot reads still succeed. Creating a Quota for a `Direct`-mode metric is permitted, and
  the Quota is counted by the `quota_for_direct_metric_total` gauge while it stays active
- [ ] Operations against a `QuotaGated` metric for which subject resolution produces zero applicable Quotas are denied
  with `Denied(violated_quota_ids=[], reason="NO_APPLICABLE_QUOTA")` per §3.2 default-deny semantics; counters are not
  mutated, no idempotency record / operation log entry is created. The behavior is uniform across all built-in Engines
  (`most-restrictive-wins` returns this directly; `cel` is expected to return the same shape — Policies that emit
  `Allowed` with an empty `debit_plan` violate Debit-Plan invariant #4 and surface a canonical `Internal` error)
- [ ] Evaluate-preview operation (`cpt-cf-quota-enforcement-fr-evaluate-preview`) returns the Decision the system would
  produce for an equivalent debit, with `preview: true` marker; counters are not mutated; no idempotency record /
  operation log entry / lease row is created; PDP and trust-boundary rules are applied identically to debit
- [ ] Per-Policy Engine timeout enforced (default 5ms); timeout surfaces a canonical `DeadlineExceeded` error
  (fail-closed); runtime CEL errors (cost-cap exceeded, type error at evaluation, malformed return record) surface a
  canonical error (`ResourceExhausted` for cost-cap; `Internal` for type/return-record errors), all fail-closed
- [ ] Quota Snapshot read (`cpt-cf-quota-enforcement-fr-quota-snapshot-read`) returns the engine-agnostic per-Quota
  state list for explicit PDP-authorized tenant/subject/metric filters; no aggregate "headline" cap/balance is computed
- [ ] Consumer and manager/operator snapshot calls return the same per-Quota state contract; both carry explicit
  targets authorized against their respective service/user-plane principals and PDP scope
- [ ] Snapshot responses carry no Quota Resolution Policy attribution (no `policy_id`, `policy_version`, `scope`,
  `engine_id`, `engine_config`, summary, or content hash); callers needing Policy attribution use
  `cpt-cf-quota-enforcement-fr-evaluate-preview` (`diagnostics.policy_id` + `policy_version`) or the Policy-read API in
  `cpt-cf-quota-enforcement-fr-quota-resolution-policy-versioning`
- [ ] A product backend can render end-user quota state by calling the S2S snapshot endpoint with authorized
  `tenant_id=T` and user `{kind,id=U}`; end users never call QE and no per-Quota visibility filtering is applied within
  the authorized applicable set
- [ ] Bulk Quota Snapshot read (`cpt-cf-quota-enforcement-fr-bulk-quota-snapshot-read`) paginates results when the
  result set exceeds page size
- [ ] Operations that would exceed a Quota's cap produce `Denied` decisions with full violator identity; counters are
  not modified
- [ ] Tenant isolation: every **per-tenant** persisted row carries `tenant_id`; cross-tenant reads/writes are rejected
  at storage and API layers. Platform-wide operator-managed entities (Quota Resolution Policies, owner projection and
  constraint contracts in `types-registry`, registered Quota Resolution Engines) are not tenant-scoped per `cpt-cf-quota-enforcement-fr-tenant-isolation`
  (§5.12) and are addressed via their own scope discriminators rather than `tenant_id`
- [ ] All API operations require authentication; unauthenticated requests are rejected before any operation
- [ ] Authorization is PDP-gated; PDP denial surfaces a canonical `PermissionDenied` error and PDP unreachable surfaces
  a canonical `ServiceUnavailable` error mid-evaluate (both fail-closed); PDP-returned constraints are applied as
  additional read filters
- [ ] Storage plugin contract is implemented by the P1 plugin built on `toolkit-db`; alternative backends are viable
  under the same plugin contract; operator selects the active plugin via configuration
- [ ] Notification plugin contract dispatches all eight event kinds (`threshold-crossed`, `period-rollover`,
  `lease-auto-released`, `lease-resolved-by-deactivation`, `quota-changed`, `quota-counter-adjusted`,
  `quota-rollback-applied`, `policy-changed`) to all registered sinks
- [ ] Notification dispatch failure does not block Quota Enforcement write operations; failures are surfaced via
  telemetry
- [ ] Single-quota evaluation operations meet p95 ≤ 100ms at 10 000 ops/sec sustained
- [ ] System sustains ≥ 100M subjects with ≥ 10 Quotas per subject (≥ 1B total Quotas) without degrading evaluation
  latency
- [ ] System maintains 99.95% monthly availability for evaluation endpoints
- [ ] Idempotency guarantee: zero double-count events under simulated retry storms (10× normal RPS, 5% retry rate, 7-day
  soak)
- [ ] Storage fault tolerance: zero committed-data loss across storage backend restarts and gateway restarts
- [ ] Recovery time: normal evaluation resumes within 15 minutes of storage backend recovery

## 10. Dependencies

| Dependency                              | Description                                                                                                                                                                                                                                                                                                         | Criticality |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------- |
| `toolkit-db`                             | Database infrastructure used by the P1 storage plugin                                                                                                                                                                                                                                                               | p1          |
| `authz-resolver`                        | Platform PDP for read and write authorization                                                                                                                                                                                                                                                                       | p1          |
| `types-registry`                        | Metric catalog plus QE abstract bases, scope discriminators, owner projections, admitted-metric traits, metric request contracts, and attached constraint contracts. Required for bootstrap and Quota/Policy writes; deliberately absent from the evaluation hot path.                                             | p1          |
| Quota Manager metric-owner mapping      | Maps each metric to its owning Gear/projection so Quota Manager creates Quotas against the stable owner identity rather than a caller-specific type.                                                                                                                                                                | p1          |
| Engine evaluator                        | Library backing the active Quota Resolution Engine. P1: a sandboxed CEL evaluator (mandatory for the built-in `cel` Engine). P2-or-later candidates: Starlark, Lua, Wasm runtimes. The `most-restrictive-wins` Engine has no external library dependency.                                                           | p1          |
| `cluster` (platform gear)               | Leader election for the sweeper singletons via `cpt-cf-quota-enforcement-contract-cluster-coordination`. The operator binds the `quota-enforcement` cluster profile to a backend in deployment YAML (standalone for one process, Postgres for multi-instance, further backends as cluster plugins land) without touching Quota Enforcement code. | p1          |

## 11. Assumptions

| Assumption                                                                                                                  | Owner                              | Validation                                                                                                               |
| --------------------------------------------------------------------------------------------------------------------------- | ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| The `toolkit-db` infrastructure is available and provisioned with sufficient capacity for Quota Enforcement's expected scale | Platform Infrastructure            | Verified at gear bootstrapping; Quota Enforcement fails to start if `toolkit-db` is unreachable                         |
| `authz-resolver` is deployed and reachable                                                                                  | Platform Infrastructure            | Verified at gateway startup via health check; gateway fails readiness if PDP is unreachable                              |
| `types-registry` is deployed and reachable for metric validation                                                            | Platform Engineering               | Verified at gateway startup; Quota create/update operations fail with an actionable error if the registry is unreachable |
| Per-tenant quota density and operation rate fit within the published NFRs at first production deployment                    | Platform Engineering               | Pre-prod load testing at 10 000 ops/sec across 100M synthetic subjects                                                   |
| Notification sinks register with the Quota Enforcement plugin contract at deployment time                                   | Per-deployment integration team    | Quota Enforcement telemetry surfaces "no notification sinks registered" warning; events are dropped silently otherwise   |
| At least one storage plugin is deployed alongside the Quota Enforcement gear                                              | Platform Infrastructure / Operator | Gateway readiness check fails when no plugin resolves                                                                    |

## 12. Risks

| Risk                                                                                                                                           | Impact                                                                                                         | Mitigation                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| ---------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Hot-key contention on a single tenant's high-RPS counter                                                                                       | Latency violations on that tenant's evaluations                                                                | P1 latency monitoring; sharded-counter design deferred to P2 if observed                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| Custom Engine footguns (infinite loops, expensive computation, malformed Debit Plans) degrade evaluation latency or threaten counter integrity | Latency violations on Policies using non-default Engines; would-be counter inconsistency if invariants slipped | Per-Policy Engine timeout (default 5ms); strict Debit-Plan invariants enforced at the Engine boundary; Engine failure or any invariant violation surfaces a platform-canonical error (per §3.4 — `DeadlineExceeded` for timeouts, `Internal` for invariant violations and engine-internal failures, `ResourceExhausted` for cost-cap exhaustion) with no counter mutation; Engine configs validated at Policy create/update (e.g., the `cel` Engine performs parse + type-check); sandboxed evaluators (no I/O); per-Engine telemetry surfaces invariant-violation rates |
| Lease TTL too long → held capacity blocks legitimate operations                                                                                | False denials                                                                                                  | Operator-configured TTL bounds; lazy expiry interpretation makes auto-release immediate                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| Notification plugin failure floods telemetry but blocks no operations                                                                          | Lost threshold notifications                                                                                   | Best-effort delivery + telemetry surface                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| Multi-quota evaluation latency grows non-linearly with assignment count per subject                                                            | NFR violations at high quota density                                                                           | P1 indexing strategy in DESIGN.md; benchmarks at 10 assignments per subject at target RPS                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| Operator-supplied Quota Metadata grows unbounded or carries semantics QE didn't anticipate                                                     | Storage bloat; classification drift; PII leakage if operator misuses the field                                 | Per-deployment configurable byte-size limit (default 4 KB canonical JSON); owner-published shape validation on write; explicit "no PII" data classification; PDP-scoped access                                                                                                                                |

## 13. Open Questions

- **Shared metric-identifier namespace across QE, Usage Collector, and `types-registry`.** P1 QE uses
  `gts.cf.qe.metric.*` for the illustrative GTS instance examples in §3.2 because no platform-wide convention has been
  ratified for the metric identifier namespace shared between QE (which references metrics in Quotas) and the Usage
  Collector (which emits usage records). Once the cross-gear convention for shared type identifiers is resolved at the
  platform level, QE revisits the namespace choice and renames references in lockstep. QE PRD/DESIGN remains
  format-agnostic in the meantime per §3.2 ("QE inherits whatever format the registry permits"). Owner: Platform
  Engineering — target resolution: pending cross-gear type-convention decision. Naming each metric beneath its single
  owning Gear is the leading candidate; this ADR records the candidate but does not decide it.
- **Built-in `cascade-priority` Engine**: cascade is expressible via `cel` in P1; should P2 ship a hardcoded
  `cascade-priority` Engine for operators who do not want to author CEL? Trade-off: convenience and lower
  CEL-attack-surface vs. coupling QE to one specific cascade variant before operator preferences are observed in
  production. — owner: Platform Engineering — target resolution: deferred — production feedback.
- **P2 Engine candidates**: which alternative Engine languages (Starlark, Lua, Wasm-loaded operator engines) earn a P2
  slot? Decision criterion: sandboxability (no I/O), determinism, predictable resource limits, fast startup, and
  demonstrated CEL coverage gaps. — owner: Platform Engineering — target resolution: P2 design discussion.
- **Burst semantics for rate Quotas (P3)**: token bucket vs sliding window vs fixed window for the future rate quota
  type, including per-tenant vs per-region smoothing axis and the interaction between burst and lease TTL. The P3
  contract enumerated in `cpt-cf-quota-enforcement-fr-quota-type-rate-declared` lists
  `(rate, burst_capacity, smoothing_window)` as the field shape but defers the bucket-vs-window choice to
  implementation. — owner: Platform Engineering — target resolution: rate-quota implementation phase.
- **Subscription-term and anniversary-aligned periods**: P1 periods are calendar-aligned UTC (`day`, `week`, `month`,
  `year`) plus non-recurring `one_time` per `cpt-cf-quota-enforcement-fr-period-semantics`. App licensing integrations
  (e.g., the platform licensing/quota integration guide for the Proctor app) need caps that reset on an agreement's
  anniversary or span a subscription term; today the only faithful representation is `one_time` plus a validity window,
  and an arbitrary anniversary reset is not representable. Candidate resolutions: a new period GTS instance carrying an
  anchor timestamp, or a Quota Manager-side rollover/re-materialization mechanism that recreates the Quota per term.
  Decision criterion: a committed app-fulfillment consumer plus the cross-gear rollover ownership question. — owner:
  Platform Engineering / Quota Manager — target resolution: first app-fulfillment integration.
- **Period-end carry-over of unused capacity**: P1 has no rollover-of-unused semantics — every period boundary resets
  `consumed` to 0 and forfeits leftover capacity, matching the licensing service P1 PRD; neither models carry. If
  concrete product demand for "unused tokens roll over to next period" emerges, ownership belongs to the
  subscription/licensing layer (Quota Manager), not QE: that layer subscribes to `period-rollover` events, computes
  carry per subscription tier, and pushes the new cap back to QE via `update_quota`. QE remains carry-policy-free.
  Decision criterion: a concrete carry use case from a licensing-tier requirement, observed in production. — owner:
  Platform Engineering — target resolution: deferred — production feedback.
- **Cap-clamp for batch-style admission (P3)**: workloads that admit "70 of the 100 instances I requested" are not
  modeled in P1 — every Quota is `enforcement_mode = hard`. If concrete batch admission use cases emerge (LLM token
  batches with partial-response semantics, scheduler bulk admissions), introduce `enforcement_mode = hard-with-clamp`
  plus an `AllowedWithClamp(quota_id, admitted_magnitude)` Decision arm. The existing per-entry
  `amount ≤ request.amount` Debit-Plan invariant already accommodates clamped magnitudes; no further invariant
  relaxation is required for the clamp result. The current
  `cpt-cf-quota-enforcement-fr-batch-debit` envelope (multiple metrics under atomic all-or-nothing in P1;
  bulk-independent partial-success deferred to P2) does not require clamp; clamp solves a different problem (single
  metric, partial admission). — owner: Platform Engineering — target resolution: P3 PRD update if a concrete batch use
  case emerges.
- **CEL-based notification policies**: P1 uses fixed `notification_thresholds` (percentages of cap) on each Quota and
  emits `threshold-crossed` events on actual upward transitions (`cpt-cf-quota-enforcement-fr-notification-plugin`). A
  P2 enhancement could let operators declare CEL predicates that gate event emission on richer signals — e.g., "emit
  only if `consumed > 0.8 * cap` AND `request.tier == 'premium'`", or "emit a `quota-counter-adjusted` only
  when the credit amount exceeds some threshold". This composes with the existing `cel` Engine plugin contract but lives
  in the notification layer, not the admission layer. Trade-off: operator authoring power vs. notification-fatigue risk
  and additional CEL evaluation cost on every counter mutation. Decision criterion: production demand observed after P1
  ships fixed thresholds. — owner: Platform Engineering — target resolution: deferred — production feedback.
- **Engine diagnostic schema standardization**: Engines currently produce free-form `diagnostics` for callers; should we
  standardize a schema across Engines so dashboards/UIs can render uniform breakdowns? — owner: Platform Engineering —
  target resolution: P2 telemetry pass.
- **Quota Metadata indexing**: P1 does not index Quota Metadata for direct query (Engines see all applicable Quotas
  anyway). Should P2 add per-tenant metadata indexing for direct lookup APIs (e.g., "find all Quotas for tenant T where
  `metadata.region=us-east-1`")? Trade-off: query convenience vs storage cost vs cache-invalidation complexity. — owner:
  Platform Engineering — target resolution: deferred — production feedback.
- **Per-resource counter axis**: resource projection contracts now answer which resource identity/properties are
  available to a Policy, but this does not decide whether resource enters the counter key. A Quota on a named resource
  is already expressible by matching request and Quota properties, and several such Quotas may legitimately share one
  `(projection_type, subject_id, metric)` key. The ceiling is explicit: properties do not build the applicable-Quota
  set, so every per-resource Quota is included in every locked read for that subject/metric and the Engine filters only
  afterward (as the separate Metadata-indexing question notes). That scales to tens of resources, not thousands. If a
  concrete consumer needs materially larger cardinality, resource should become a counter-key component so counters
  materialize per resource on first use; a property convention cannot create counters. Decision criterion: a concrete
  consumer plus an order-of-magnitude resource-count estimate. — owner: Platform Engineering — target resolution:
  demand-driven design.
- **Deactivated-Quota auto-purge**: P1 retains deactivated Quotas indefinitely for read access (no automatic delete).
  Should P2 introduce a per-tenant operator-configurable grace-period auto-purge (e.g., "hard-delete deactivated Quotas
  after 90 days") tied to audit infrastructure? Trade-off: storage growth on long-lived tenants vs audit/compliance
  trail durability. — owner: Platform Engineering / Compliance — target resolution: P2 audit infrastructure rollout.
- **Grace policies for hard caps**: detailed design for "+N% for T minutes" grace windows on `enforcement_mode = hard`
  Quotas (e.g., during a campaign or an incident, allow 110% of cap for the next 30 minutes). P2 enhancement; orthogonal
  to the rate-quota burst contract since grace applies to consumption Quotas while burst applies to rate Quotas. —
  owner: Platform Engineering — target resolution: P2 enhancement.
- **EventBus integration**: Routing semantics, delivery guarantees, and migration plan from in-process notification
  plugin to platform EventBus. — owner: Platform Infrastructure — target resolution: when EventBus availability is
  committed.
- **Audit design**: Schema and storage destination for definition-change audit and consumption-event audit, including
  retention and PII-handling expectations. — owner: Platform Engineering / Compliance — target resolution: P2 audit
  infrastructure rollout.
- **Multi-source Quotas and subscriber-customizable thresholds (P2)**: P1 ships a two-value `source` enum on every Quota
  — `licensing` (materialized by the licensing layer) and `operator` (manual caps for incidents and compliance) — with
  **uniform** mutation rules across both values (operator-level or Quota-Manager PDP scope). P2 introduces a Quota
  Source Registry that extends the enum additively with `tenant_admin` (tenant-administrator-imposed caps on subjects
  within their tenant — for example a per-user sub-cap inside a tenant license) and `user_self` (end-user
  self-protection caps, e.g., a Pay-as-you-go tenant limiting their own monthly spend). Each P2-introduced source kind
  declares the PDP scope required to mutate it and any cap-inheritance constraint (e.g., a `user_self` cap MUST NOT
  exceed the smallest applicable license cap for the same metric and subject). P2 also introduces
  subscriber-customizable notification thresholds via an auto-created `NotificationSubscription` per Quota: at Quota
  creation time the system automatically materializes a Subscription whose subscriber is the Quota's subject and whose
  thresholds are seeded from the Quota's `notification_thresholds` field. Subscribers MAY mutate, extend, or fully
  deactivate their auto-created Subscription to silence noisy default events or add custom thresholds — without
  modifying the underlying Quota. Concrete FRs and the corresponding schema additions land in the P2 PRD revision. —
  owner: Platform Engineering — target resolution: P2 PRD revision.
- **Hierarchical quota lookup as an additive extension**: P1 builds the applicable-Quota set per-
  `(projection_type, subject_id)` independently — no traversal of any subject hierarchy, no consumption against an
  ancestor's Quota. A future extension is expected to introduce hierarchy-aware lookup as a strictly additive change:
  an individual Quota opts in via a marker on the Quota itself, and evaluations targeting a descendant of that Quota's
  subject pick it up as part of the applicable set (potentially also propagating the debit up the chain). Open design
  points: (1) the marker's shape — a typed enum on the Quota record vs a reserved `metadata` key vs a
  capability on an owner projection contract that opts a whole projection scope in; (2) where the hierarchy itself lives and how the
  applicable-set builder discovers ancestors at evaluation time without inflating latency; (3) how ancestor walk
  composes with the Engine contract without altering `Decision` arms or Debit Plan invariants; (4) atomic multi-Quota
  acquisition (`cpt-cf-quota-enforcement-fr-lease-acquire`) across N ancestor counters under deadlock-free ordering; (5)
  period attribution when ancestor and descendant Quotas carry different period specs; (6) idempotency / rollback
  fan-out across the ancestor chain; (7) PDP scope for "descendant consuming against an ancestor's Quota". Decision
  criterion: concrete product demand for organization-tree consumption accounting plus a credible additive-shape
  proposal that preserves P1 behavior for non-opted-in Quotas. — owner: Platform Engineering — target resolution:
  deferred — production feedback.
- **Composable Policy patterns (P2)**: introduce a Pattern Registry parallel to projection contracts — operators select
  a parameterized pattern (`cascade`, `attribute_gated`, `most_restrictive_wins`, `cel` escape hatch) instead of
  authoring raw CEL for every Policy. Reduces operator-authored CEL footprint to the genuine long-tail. Patterns have
  proven cumulative behavior, statically-checkable parameter shapes, and are auditable at a glance. Concrete FR lands in
  P2 PRD revision. — owner: Platform Engineering — target resolution: P2 PRD revision.
- **Shadow evaluation for Policy promotion (P2)**: when an operator stages a new Policy version, it runs in shadow mode
  alongside the active version; both produce Decisions; production enforcement uses the active one; shadow Decisions are
  recorded for divergence analysis. Operator promotes (latest-pointer to shadow version) when divergence is acceptable;
  rollback is one click via the versioning primitive (`cpt-cf-quota-enforcement-fr-quota-resolution-policy-versioning`).
  Concrete FR lands in P2 PRD revision. — owner: Platform Engineering — target resolution: P2 PRD revision.
- **Narrower-than-metric Policy scope**: P1 supports only `global` and `metric` scopes for Quota Resolution Policy
  (`cpt-cf-quota-enforcement-fr-quota-resolution-policy`). Real-world demand may emerge for narrower targeting —
  per-`projection_type` (different arbitration for owner tenant vs user scopes), per-specific-subject
  (premium-tenant carve-outs, surgical workarounds, A/B per-tenant Policy testing), or some other axis (e.g., per
  Quota-metadata attribute). The right shape is unclear: per-`projection_type` alone has a tie-break problem when an
  evaluation's applicable-subjects set spans several types simultaneously; per-`(projection_type, subject_id)` avoids the
  ambiguity but adds storage and operator-management cost; pinning to `quota_id` blurs the Quota-vs-Policy separation
  given that multiple Quotas can share `(subject, metric)`. The persistence and selection layers should be designed to
  admit additional scope levels without breaking changes (so a future addition does not require a Policy data
  migration), but no narrower scope is exposed in P1. Decision criterion: concrete operator use cases observed in
  production after P1, plus alignment with the related Composable Policy Patterns and Shadow Evaluation Open Questions
  (which may absorb some of the same use cases without expanding the scope ladder). — owner: Platform Engineering —
  target resolution: deferred — production feedback.
- **Policy unit-test framework (P3)**: operators declare `(EvaluationContext, expected Decision)` test cases stored
  alongside Policy; on create/update, the system runs the tests via the existing
  `cpt-cf-quota-enforcement-fr-evaluate-preview` infrastructure; failure rejects the change with actionable diff.
  Composes with versioning (each version has its own tests) and patterns (test obligation concentrates on residual CEL
  escape-hatch surface, not on parameterized templates whose properties are already proven). Deferred to P3 behind
  patterns (P2) because patterns reduce the volume of operator-authored CEL most needing coverage. Concrete FR lands in
  P3 PRD revision. — owner: Platform Engineering — target resolution: P3 PRD revision.
- **Constrained quota-arbitration DSL (post-P3 / open)**: design a domain-specific language for quota arbitration —
  statically typed, decidable, no loops/recursion, formally verifiable. Replaces CEL footprint over time as an
  alternative Engine. Decision criterion to revisit: scale of operator-authored CEL observed in P2/P3 production; if
  footprint stays small (most operators stay on patterns), CEL escape hatch remains adequate. If footprint grows
  unmanageable, design + prototype a constrained DSL. Indefinite timeline; re-evaluated yearly post-P3. — owner:
  Platform Engineering — target resolution: post-P3, conditional on production evidence.

## 14. Traceability

- **Informal upstream requirements**: no formal `UPSTREAM_REQS.md` is maintained for this gear
- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: see [DESIGN §5 Traceability](./DESIGN.md) for the canonical ADR catalogue
- **Features**: feature specifications under `docs/features/` (eleven documents, one per DECOMPOSITION entry)
- **Related gears**: [Usage Collector PRD](../../usage-collector/docs/PRD.md),
  [Account Management PRD](../../account-management/docs/PRD.md),
  [Resource Group PRD](../../resource-group/docs/PRD.md), [Types Registry PRD](../../types-registry/docs/PRD.md)
