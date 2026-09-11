<!-- CONFLUENCE_TITLE: [BSS]: Orders Lifecycle — Technical Design -->
<!-- Related: ./PRD.md, ./design/, ../orders-workflow/docs/PRD.md | Owners: BSS Orders team -->

# Technical Design — Orders Lifecycle


<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Principles & Constraints](#2-principles--constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Domain Model](#31-domain-model)
  - [3.2 Component Model](#32-component-model)
  - [3.3 API Contracts](#33-api-contracts)
  - [3.4 Internal Dependencies](#34-internal-dependencies)
  - [3.5 External Dependencies](#35-external-dependencies)
  - [3.6 Interactions & Sequences](#36-interactions--sequences)
  - [3.7 Database schemas & tables](#37-database-schemas--tables)
  - [3.8 Deployment Topology](#38-deployment-topology)
- [4. Additional context](#4-additional-context)
  - [4.1 Capacity and cost](#41-capacity-and-cost)
  - [4.2 Security posture](#42-security-posture)
  - [4.3 Data protection, residency and retention](#43-data-protection-residency-and-retention)
  - [4.4 Observability](#44-observability)
  - [4.5 Error handling and the outbox failure posture](#45-error-handling-and-the-outbox-failure-posture)
  - [4.6 Testability](#46-testability)
  - [4.7 Extension and provenance](#47-extension-and-provenance)
- [5. Traceability](#5-traceability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-design-orders-lifecycle`
## 1. Architecture Overview

### 1.1 Architectural Vision

Orders Lifecycle is the **System of Record** for the order document and its finite-state
machine. It owns WHAT was ordered — line items, parties, pricing references — and the CURRENT
state of the order from capture to a terminal state. It never computes a price, never routes an
approval, and never provisions anything ([`PRD.md`](./PRD.md) §1.1, §6.4).

The design follows the shape its two built BSS siblings already use. Where the Billing Ledger's
contract is *post through the engine* (build balanced lines, then commit) and the Product
Catalog's is *publish through the engine* (author draft, validate fail-closed, freeze, emit),
this gear's contract is **transition through the engine**. One shared **Order Transition
Engine** ([`design/01-foundation`](./design/01-foundation.md)) owns the order aggregate and
its append-only version chain, the state-machine table and its guards, the idempotency
registry, the optimistic version check, the transition audit log, and the event outbox. Every
state change — a buyer submitting, an operator holding, the sibling Workflow gear reflecting an
approval, the scheduler expiring a stale order — enters through the same engine call and leaves
having done exactly four things atomically: the state or version change, one audit entry, one
settled idempotency record, and one outbox row where the transition row declares an event type.

Each business capability is a **slice handler** that declares its guard predicates and its
contribution to the order document, then transitions *through* the Engine under the invariants
defined there. The Engine owns no commercial policy — it does not know what a sellability gate
or a catalog price pin is; slices own no transition mechanics — they never write state, never
stamp an audit row, and never emit an event themselves. This keeps the correctness-critical
core (idempotency, versioning, guard evaluation, audit completeness, one-event-per-transition) small
and auditable, and it is what makes the four `p1` non-functional guarantees provable in one
place rather than argued per capability.

Two boundaries are structural rather than stylistic, and both are stated normatively in
[`PRD.md`](./PRD.md) §6.4. The sibling **Orders Workflow** gear drives every approval and
fulfillment transition by calling this gear idempotently and stores no authoritative order
state (R1); and all provisioning reaches OSS only through Subscriptions (R3), so this gear
holds no provisioning logic and learns fulfillment outcomes exclusively from Workflow
acknowledgements. Requirements (WHAT/WHY) live in [`PRD.md`](./PRD.md); per-capability
mechanics live in [`design/`](./design/).

### 1.2 Architecture Drivers

Requirements that significantly influence architecture decisions.

#### Functional Drivers

| Requirement | Design Response |
|-------------|------------------|
| `cpt-cf-bss-orders-lifecycle-fr-order-state-machine` | The state machine is a declarative transition table owned by the Engine (states, guards, terminal set, hold/resume, expiry eligibility). Slices register guards; no slice may add an edge. |
| `cpt-cf-bss-orders-lifecycle-fr-order-idempotency` | An Engine-owned idempotency registry keyed per operation, storing the committed outcome. Replay returns the stored result — including a stored failure; payload mismatch and in-flight arrival are distinct refusals, never a success. |
| `cpt-cf-bss-orders-lifecycle-fr-order-create` | The capture slice authors order and line state in `draft` with no gate evaluation, so basket workflows cost nothing until submit. |
| `cpt-cf-bss-orders-lifecycle-fr-order-submit` | The gate-and-pin slice adopts the published pricing sellability predicates by reference and adds the Orders delta; the Engine commits `draft → submitted` only on a fully passing gate, so no partially validated order can exist. |
| `cpt-cf-bss-orders-lifecycle-fr-order-line-dates` | Line dates, term duration and billing cycle are line-level authored fields with cascading defaults; expected fulfillment time is derived, never stored as authority. |
| `cpt-cf-bss-orders-lifecycle-fr-order-amendment` | Commercial content is immutable from `submitted`: an amendment appends a new version row with a `supersedesVersion` back-reference and re-runs the gate. Administrative content is a separate, non-versioned, audited edit path. |
| `cpt-cf-bss-orders-lifecycle-fr-order-history` | Versions are append-only rows retained in-table, so any historical version is retrievable by order id and version number without reconstruction. |
| `cpt-cf-bss-orders-lifecycle-fr-order-tenant-axes` | The three axes are validated at submit and then frozen by the Engine's guard set; only `payerTenantId` has an amendment path, and it carries the paired payer/seller rebinding predicate. |
| `cpt-cf-bss-orders-lifecycle-fr-order-acceptance` | The acceptance instant is a first-class recorded fact with its own transition and event, and a begin-fulfillment guard reads it. It has no default value at any layer. |
| `cpt-cf-bss-orders-lifecycle-fr-order-payment-auth` | Payment authorization is consumed as a begin-fulfillment guard input supplied by Workflow, not as an order state; the tolerate-failure election is a seller policy read at guard time. |
| `cpt-cf-bss-orders-lifecycle-fr-order-cancel` | The cancel guard is anchored on the recorded subscription-spawn signal, which is why begin-fulfillment must be durably committed before Workflow issues any activation intent. |
| `cpt-cf-bss-orders-lifecycle-fr-order-hold` | `on_hold` stores the pre-hold state on the order, so resume is a table lookup rather than an inference. |
| `cpt-cf-bss-orders-lifecycle-fr-order-expiry` | A coordinated scheduler drives per-state TTL expiry as an ordinary Engine transition; `in_fulfillment` and holds taken from it are excluded by the transition table itself, not by scheduler logic. |
| `cpt-cf-bss-orders-lifecycle-fr-order-atomic-fulfillment` | The order carries no per-line fulfillment state machine. Terminals are order-level; per-line create/activate results are a read-only projection fed by Workflow acknowledgements. |
| `cpt-cf-bss-orders-lifecycle-fr-order-subscription-linkage` | The per-line line→subscription mapping is persisted on acknowledgement and carried in `OrderCompleted`, so acquisition provenance is answerable from the order side. |
| `cpt-cf-bss-orders-lifecycle-fr-order-events` | The eleven state events are enqueued from an Engine-owned outbox inside the transition commit, giving exactly one outbox row per committed transition **that declares an event type**, under at-least-once delivery with consumer de-duplication. Six row classes are deliberately event-less (D-15). |
| `cpt-cf-bss-orders-lifecycle-fr-order-authorization` | Per-actor permissions and the cross-tenant delegation-proof requirement are enforced as an Engine pre-guard, so no slice can widen scope. |
| `cpt-cf-bss-orders-lifecycle-fr-orders-boundary-r1-state-sor` | Workflow-only operations are ordinary Engine transitions with the same idempotency and version-check contract as buyer operations; the gear exposes no path that lets a caller assert state without a guard. |
| `cpt-cf-bss-orders-lifecycle-fr-orders-boundary-r2-approval` | The approval-requirement verdict and gate outcomes are stored as received values with their deciding authority recorded. The gear contains no policy evaluation and no threshold comparison. |
| `cpt-cf-bss-orders-lifecycle-fr-orders-boundary-r3-provisioning` | No infrastructure adapter for OSS or the Policy Engine exists in this gear. Fulfillment outcome arrives only as a Workflow acknowledgement. |
| `cpt-cf-bss-orders-lifecycle-fr-orders-boundary-r4-no-price` | Price data is stored as opaque references plus the captured pin and the captured non-authoritative total. The gear has no arithmetic over money beyond persistence at ISO 4217 minor-unit scale. |
| `cpt-cf-bss-orders-lifecycle-fr-orders-boundary-r5-no-mirroring` | The persisted downstream transition-request identifier is a correlation column with no state semantics, and no projection derives order state from it. |

#### NFR Allocation

| NFR ID | NFR Summary | Allocated To | Design Response | Verification Approach |
|--------|-------------|--------------|-----------------|----------------------|
| `cpt-cf-bss-orders-lifecycle-nfr-order-transition-latency` | Transition commit p95 < 1 s. **Two disclosed divergences**: the PRD's threshold reads "durable write **+ event publish**", and the outbox makes publication asynchronous by construction — delivery carries its own **30 s p95** budget (D-41), so the combined figure cannot hold as written; and port resolution adds up to 1.5 s ahead of the commit (1.75 s on Preview), leaving the caller-visible figure unbounded (Q-11). Both routed to Product as Q-16. | Order Transition Engine | Single-round-trip transition: guard evaluation, version/audit append and outbox enqueue in one database transaction; delivery is asynchronous from the outbox so publication never extends the commit path | Load test at production sizing asserting p95 on the commit path; outbox drain measured separately against the event-delivery budget |
| `cpt-cf-bss-orders-lifecycle-nfr-order-read-latency` | Order read and paginated list p95 < 200 ms | Read-and-authorization slice | Reads are served from a current-version projection carrying the denormalized state, tenant axes and per-line fulfillment status, so no read reconstructs the version chain | Read/list benchmarks at production row counts and page sizes, including the tenancy-scoped filter paths |
| `cpt-cf-bss-orders-lifecycle-nfr-order-audit-completeness` | 100 % of transitions and amendments audited, zero silent drops | Order Transition Engine | The audit append is inside the same transaction as the state change, so an unaudited transition cannot commit; the audit store is append-only and the chain is verifiable | Structural test that every transition-table edge writes an audit row; negative test that a failed audit append aborts the transition |
| `cpt-cf-bss-orders-lifecycle-nfr-order-idempotency` | Zero duplicate orders or duplicate transition effects, **per principal** (`design/01-foundation.md` §4.2 discloses the scope's one gap: create) | Order Transition Engine | Idempotency records are written in the transition transaction under a unique constraint on `(operation, principal_scope, idempotency_key)`, making duplicate effect impossible rather than unlikely; the in-flight state is explicit | Concurrency test firing the same key in parallel and asserting one durable effect; replay test asserting stored failures replay as failures; cross-principal test asserting one caller neither reads nor overwrites another's record |
| `cpt-cf-bss-orders-lifecycle-nfr-order-snapshot-integrity` | 100 % of submitted lines carry a resolvable catalog price pin | Gate-and-pin slice | The pin is captured inside the submit transaction; a line without a resolvable pin fails the gate, so `submitted` and "pinned" are the same commit | Invariant test asserting no `submitted`-or-beyond line exists without a pin; re-pin asserted on every amendment |
| `cpt-cf-bss-orders-lifecycle-nfr-order-recovery` | RPO zero for `submitted`+ orders, RTO ≤ 60 min, within residency-bound intra-cell failure domains | Persistence and deployment topology | Committed transitions are synchronously durable before acknowledgement; versions, audit rows and outbox entries share the transaction so a recovered database cannot hold a state change without its trail | DR exercise restoring to the declared RTO and asserting zero committed-transition loss including outbox entries |
| `cpt-cf-bss-orders-lifecycle-nfr-order-retention` | Retain all orders and versions per program policy; auto-void abandoned drafts | Persistence and the hold-and-expiry slice | Append-only retention with no destructive path for `submitted`+ orders; the draft auto-void TTL transitions to `expired` rather than deleting, preserving the audit trail | Retention test asserting no delete path reaches a `submitted`+ order; auto-void test asserting the draft remains readable, not removed |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|-----------------|
| `cpt-cf-bss-orders-lifecycle-adr-transition-through-engine` | One engine owns every state change, so the four `p1` guarantees are properties of one code path rather than per-capability discipline |
| `cpt-cf-bss-orders-lifecycle-adr-slice-decomposition` | A foundation slice plus seven capability slices, so the correctness core has an independent review boundary |
| `cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate` | An unevaluable gate input is a refusal, not an admission under a tolerated risk — so no order is pinned against a predicate nobody checked |
| `cpt-cf-bss-orders-lifecycle-adr-closed-enumerations` | The eleven states and eleven events stay closed; new distinctions are guards, recorded facts, reasons and event-less rows |
| `cpt-cf-bss-orders-lifecycle-adr-refusals-commit` | A refused transition audits, settles and commits, which is what makes 100 % audit coverage a property rather than a discipline |
| `cpt-cf-bss-orders-lifecycle-adr-outbox-publication` | Events publish asynchronously from a transactional outbox, which is why the PRD's combined "durable write + event publish" threshold cannot hold and delivery carries its own budget |
| `cpt-cf-bss-orders-lifecycle-adr-in-transaction-concurrency` | The one-in-flight-order rule is a database constraint inside the transition transaction, not a gate predicate — the predicate is a pre-check that cannot enforce it |

**Seven ADRs, and the reasoning for that number.** An ADR is written where a decision affects the
system's fundamental structure, is hard to reverse, and represents a real choice between
alternatives. For every other entry the register — which carries a decision, its
rationale and its propagation addresses — is the correct and sufficient home; an ADR per decision
would destroy the signal that makes an ADR directory readable. Seven is above the sibling gears'
one to three because Orders is the only state-machine system of record among them and the only one
with three consuming gears. The test above is applied rather than recited: the two added on
2026-09-10 were found by asking which decisions the register was carrying that met all three
conditions, and both did while being recorded as single table rows. **Asynchronous publication**
(`ADR/0006`) makes a PRD acceptance criterion unsatisfiable by construction and binds three
consumer gears to at-least-once delivery. **In-transaction concurrency** (`ADR/0007`) is the gear's
only concurrency-correctness mechanism, and a table row with no alternatives is exactly the shape a
later author deletes as redundant with the gate predicate that does not enforce it. Every other call is in
[`DECISIONS.md`](./DECISIONS.md).

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-orders-lifecycle-tech-layering`

```text
Capability slices   capture · gate-and-pin · versioning · preconditions ·
(commercial policy) workflow-seam · hold-and-expiry · read-and-authz
       │   declare guard predicates and document contributions; transition through the
       ▼            Engine API — own no state write, audit row, or event emission
Order Transition    order aggregate · append-only version chain · state-machine table + guards ·
Engine              idempotency registry · optimistic version check · transition audit ·
(shared engine)     event outbox (11 state events) · machine-readable reason catalogue
       │            — owns no commercial policy
       ▼
Persistence         toolkit-db backend (append-only version rows; current-version read
                    projection; append-only audit store; idempotency registry; event outbox;
                    resolved totals as integer minor units at ISO 4217 scale)
```

| Layer | Responsibility | Technology |
|-------|---------------|------------|
| Presentation | REST order authoring, transition, preview and read surfaces behind the inbound gateway; `OperationBuilder`-registered operations with explicit response metadata; RFC 9457 `application/problem+json` errors; ETag optimistic concurrency | Rust, REST/OpenAPI, inbound API gateway |
| Application | Capability slices declaring guards and document contributions; each is a bounded feature owning its own validation and its own machine-readable reasons | Rust modules in the `orders-lifecycle` gear |
| Domain | The Transition Engine: aggregate and version chain, transition table and guard evaluation, idempotency semantics, version check, audit and outbox contracts | Rust; GTS for the cross-gear contract surface (`01 §4.7`) + Rust domain structs |
| Infrastructure | Append-only version and audit stores, current-version projection, idempotency registry, event outbox, expiry scheduler | PostgreSQL, SecureORM, coordination lease library |

## 2. Principles & Constraints

### 2.1 Design Principles

#### Transition through the engine

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-transition-through-engine`

Every order state change is a single Engine call that atomically evaluates the guard, appends
the version or audit entry, commits, and enqueues one outbox row **where the transition row
declares an event type** — six row classes are deliberately event-less (`01 §4.4`, D-15). No slice, migration,
repair script or administrative surface writes order state directly. This is what allows audit
completeness, idempotency and one-outbox-row-per-event-declaring-transition to be asserted once
rather than per capability — delivery itself is **at-least-once** with consumer de-duplication
(`01 §2.2`), never exactly-once
— and it is the reason a new capability cannot regress the correctness core by construction.

#### One state authority, no derived truth

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-single-state-authority`

Order state is stored, not computed. No projection, event replay or downstream identifier
derives it, and the persisted Subscriptions transition-request identifier carries no state
meaning. The corollary bounds this gear from the other side: it stores approval verdicts and
fulfillment outcomes as *received facts with a recorded decider*, and evaluates neither.

#### Commercial content is append-only; administrative content is editable

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-content-immutability-split`

From `submitted` onwards, line items, quantities, plan and price references, tenant axes, dates,
term and category change only by appending a new version. External references, display labels
and internal notes are edited in place and audited separately. The split is enforced at the
field level in the document contribution contract, not by reviewer discipline, because
conflating the two is what makes a commercial audit trail unreliable.

#### Fail closed on absence

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-fail-closed`

An unevaluable predicate is a failed predicate. A submit whose sellability inputs cannot be
resolved is refused, not defaulted — the same posture the published pricing gate takes, where an
unbuilt predicate lane is treated exactly as one that timed out. No layer substitutes a default
for an absent required value; absence must have failed the gate.

#### Reasons are business-level and machine-readable

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-machine-readable-reasons`

Every refusal — gate rejection, guard violation, payload mismatch, stale version, market
divergence, overlap collision — carries a stable machine-readable business reason owned by the
slice that raises it, mapped to an RFC 9457 problem at the wire edge. Transport status codes are
a presentation concern and never the contract a caller keys on.

#### Idempotency is a stored outcome, not a de-duplication guess

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-stored-idempotency`

The registry stores the committed outcome of an operation, so replay returns that outcome —
including a stored failure — rather than re-executing or optimistically assuming success. The
three non-success cases are distinct and none of them may be read as success: payload mismatch,
still-processing conflict, and stale version.

### 2.2 Constraints

#### Approval policy is external and currently unimplemented

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-external-approval-policy`

The approval-requirement verdict is owned by the Generic Approval service (R2), which has no
authored specification. This gear therefore stores a verdict it cannot validate, and must record
the deciding authority alongside it so a stand-in decision is distinguishable from a policy
decision after the fact. No fallback evaluation may be added here, and the built BSS siblings'
local approval surfaces are not a precedent this gear may follow.

#### Downstream provisioning contracts are unagreed

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-unagreed-subscription-seams`

The overlap-presence read the submit gate needs, and the compensation cancel reason the failure
path depends on, are asks on the Subscriptions gear that are registered but not agreed, and that
gear has no implementation. The gate and the acknowledgement path are therefore designed against
a specified contract rather than an observed one, and each dependency is isolated behind a port
so an upstream change is a boundary change and not a core change.

#### Payment ordering is provision-then-collect only

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-payment-ordering`

Only an authorization outcome consumed as a begin-fulfillment guard is expressible; capture,
strong customer authentication and refund-as-reversal have no owning capability in the platform.
A collect-then-provision checkout cannot be built on this design without extending it, and the
gear introduces no `payment_pending` state, so a declined instrument leaves the order `approved`
until its TTL elapses.

#### Money is stored but never computed

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-no-money-arithmetic`

The resolved total is persisted as integer minor units at the currency's ISO 4217 scale and is
read only for display and for the approval-request context Workflow assembles. The gear performs
no derivation, aggregation or currency conversion over it, and it is never a billing input.

#### Data residency is a hard boundary

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-data-residency`

For residency-bound tenants every gear-owned store — tables, read projection, audit, idempotency
registry, outbox, backups and the synchronous standby — is pinned to an in-jurisdiction deployment
cell with **zero cross-boundary replication**. This is what forces the recovery standby to be a
second failure domain rather than a second region, and it is the constraint the sibling catalog
gear states in the same terms.

#### Constraint categories not applicable

- [ ] `p3` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-categories-not-applicable`

Two checklist constraint categories are recorded as inapplicable rather than omitted. **Vendor and
licensing**: the gear introduces no third-party dependency beyond the platform's own ToolKit,
PostgreSQL and the coordination lease library, all already licensed platform-wide. **Resource
constraints** — budget, team size, delivery window: these are project-level facts owned outside
the design set and would date immediately if restated here; the design's own sequencing
constraint is the phased slice map in [`design/README.md`](./design/README.md).

#### Platform baselines apply without deviation

- [ ] `p3` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-platform-baselines`

The gear takes the standard ToolKit posture with no requested deviations: SDK-first public
contracts in an `orders-lifecycle-sdk` crate with implementation internals private to the gear
crate; `api` / `domain` / `infra` separation; `OperationBuilder` registration with explicit
response metadata; canonical error mapping to RFC 9457 with no internal diagnostics on the wire;
runtime-owned database privilege with the gear exposing migrations and receiving scoped access;
and `SecurityContext` propagated across every in-process call.

## 3. Technical Architecture

### 3.1 Domain Model

**Technology**: GTS types for cross-gear contracts, specified in [`design/01-foundation.md`](./design/01-foundation.md) §4.7; Rust domain structs internally.

**Location**: [`design/01-foundation`](./design/01-foundation.md) §3.1 is normative for the
aggregate and its invariants.

**Core Entities**: each carries its own stable ID; the normative definitions and the schema they
map to are in [`design/01-foundation`](./design/01-foundation.md) §3.1 and §3.7.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-order-aggregate`

`Order` — the aggregate root: identity, human-readable number, category, the three tenant axes,
the initiating actor, the optional contract reference, current state, `state_entered_at`, the
current-version pointer, the pre-hold state, the spawn-signal instant, the tolerated-authorization
risk flag and the audit-sequence counter.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-order-version`

`OrderVersion` — an immutable snapshot of commercial content with its actor, timestamp, reason,
derived order market and `supersedesVersion` back-reference.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-order-line`

`OrderLine` — a version-scoped line under an order-scoped `OrderLineIdentity`: catalog references,
quantity, currency, catalog price pin, the resolved date triple with its governing policy-switch
state, term, cycle and overlap scope key.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-resolved-total-view`

`ResolvedTotal` — the captured non-authoritative figures, discriminated by line or order scope,
across four charge kinds.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-transition`

`OrderTransition` — one hash-chained append-only audit entry per transition attempt, committed or
refused.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-acceptance`

`AcceptanceRecord` — the customer-acceptance instant as a recorded fact, never defaulted, with its
recording actor, path and requirement source.

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-line-fulfillment`

`LineFulfillment` — the read-only per-line projection of Workflow acknowledgements plus the
spawned subscription identifier and the downstream correlation reference.

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-administrative-content-view`

`AdministrativeContent` — mutable, separately audited content carrying no commercial meaning:
external references, display labels and internal notes at order and line level.

**Relationships**:
- `Order` → `OrderVersion`: one-to-many, append-only; exactly one version is current, and every prior version is retained and retrievable.
- `OrderVersion` → `OrderLine`: one-to-many; lines belong to a version, not to the order, which is what makes commercial content immutable without copying the aggregate.
- `OrderVersion` → `ResolvedTotal`: one-to-many — one row per line per charge kind plus the order-level roll-up; captured at submit and recaptured on each amendment.
- `Order` → `OrderTransition`: one-to-many, append-only; the audit trail is complete by construction because the append shares the transition's transaction.
- `Order` → `AcceptanceRecord`: zero-or-one; present only where acceptance was required and recorded, and never defaulted.
- `OrderLine` → `LineFulfillment`: one-to-one after fulfillment acknowledgement; carries the 1:1 line-to-subscription mapping.

### 3.2 Component Model

Components are slice handlers over the shared Transition Engine, not independently deployable
services. Each carries a stable `cpt-cf-bss-orders-lifecycle-component-{slug}` ID; the linked
slice document is normative for its internals, and the dependency order is in
[`design/README.md`](./design/README.md).

```mermaid
graph TB
    subgraph Slices["Capability slices - commercial policy"]
        CAP[capture]
        GATE[gate-and-pin]
        VER[versioning]
        PRE[preconditions]
        SEAM[workflow-seam]
        HOLD[hold-and-expiry]
        READ[read-and-authz]
    end
    ENG[Order Transition Engine]
    DB[(toolkit-db)]
    OUT[Event outbox]
    CAP --> ENG
    GATE --> ENG
    VER --> ENG
    PRE --> ENG
    SEAM --> ENG
    HOLD --> ENG
    READ --> ENG
    ENG --> DB
    ENG --> OUT
```

#### Order Transition Engine

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-transition-engine`

##### Why this component exists

The four `p1` guarantees this gear is judged on — audit completeness, zero duplicate effects,
transition latency and recoverability — are properties of *how a state change commits*, not of
any single capability. Concentrating the commit in one component makes them assertable once and
unbreakable by a new slice.

##### Responsibility scope

The order aggregate and its append-only version chain; the declarative state-machine table with
its guards, terminal set, hold/resume mapping and expiry eligibility; guard evaluation and
ordering; the idempotency registry and its non-success outcomes; the optimistic version
check and its `version-conflict` refusal — the single registered name D-38 consolidated the
`stale-version` variants into; the append-only transition audit; the event outbox and the
one-event-per-committed-transition rule; and the registry of machine-readable business reasons.

##### Responsibility boundaries

It knows nothing commercial: not what a sellability predicate is, not what a price pin means,
not whether an approval was warranted. It evaluates guards that slices declare, over document
contributions that slices supply. It performs no money arithmetic, no policy evaluation, no
provisioning, and no outbound call to any other gear.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-capture` — owns data for
- `cpt-cf-bss-orders-lifecycle-component-gate-and-pin` — owns data for
- `cpt-cf-bss-orders-lifecycle-component-versioning` — owns data for
- `cpt-cf-bss-orders-lifecycle-component-workflow-seam` — owns data for
- `cpt-cf-bss-orders-lifecycle-component-hold-and-expiry` — owns data for

#### Capture handler

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-capture`

##### Why this component exists

A basket must be assemblable without paying validation cost, and the line model must carry the
quoted commercial shape of the deal — term and cycle — or that shape is lost between order and
subscription.

##### Responsibility scope

Order and line authoring in `draft`; the line model including the mandatory contract-effective
date, the optional service-activation and acceptance-due dates with their cascading defaults,
term duration, billing cycle and external references; the single-currency basket rule; and the
field-level classification of commercial versus administrative content.

##### Responsibility boundaries

It evaluates no sellability predicate, captures no pin, and computes no total — a `draft` is
deliberately unvalidated. It does not decide whether a missing required date blocks submit; that
guard belongs to the gate.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-engine` — depends on
- `cpt-cf-bss-orders-lifecycle-component-gate-and-pin` — shares model with

#### Gate-and-pin handler

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-gate-and-pin`

##### Why this component exists

Price integrity between capture and subscription activation is the revenue-integrity risk this
gear exists to close, and the gate is the only place it can be closed atomically with the state
change.

##### Responsibility scope

The submit gate: the published pricing sellability predicates adopted by reference plus the
Orders delta — tenant-axis validity, contract-active where referenced, purchase-quantity floor,
order-market consistency against the payer's profile, reference resolution, single currency,
overlap-rule uniqueness and the one-in-flight-order rule. Capture of the catalog price pin on
every line and of the non-authoritative resolved total. The Preview operation, which creates no
order or commercial artifact, persists bounded-retention gate outcomes and returns no approval
verdict.

##### Responsibility boundaries

It does not author the adopted predicates and must never fork them. It computes no price — the
total arrives from the price-evaluation contract and is stored as received. It returns no
approval-requirement verdict, in Preview or at submit.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-engine` — depends on
- `cpt-cf-bss-orders-lifecycle-component-capture` — shares model with

#### Versioning handler

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-versioning`

##### Why this component exists

A commercial change before fulfillment must leave evidence of what changed, who changed it and
what it replaced, without mutating what a reviewer already saw.

##### Responsibility scope

The amendment path from `submitted`, `pending_approval` and `approved`; the new-version append
with its `supersedesVersion` reference; the gate re-run and re-pin trigger; historical version
retrieval; the non-versioned audited administrative edit path; and the amendment-forbidden guard
from `in_fulfillment` onward.

##### Responsibility boundaries

It does not decide whether the amended order needs re-approval — that verdict is external and
arrives through the workflow seam. It does not delete or rewrite a prior version under any
condition.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-engine` — depends on
- `cpt-cf-bss-orders-lifecycle-component-gate-and-pin` — calls

#### Preconditions handler

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-preconditions`

##### Why this component exists

On a partner-placed order the document evidences delegation but not agreement, and without a
money gate before provisioning a non-paying tenant receives resources.

##### Responsibility scope

The customer-acceptance instant as a recorded fact with its own transition and event; the source
of the acceptance-required election; and the begin-fulfillment guard inputs — recorded acceptance
where required, and the payment-authorization outcome with the seller tolerate-failure election
and its risk flag.

##### Responsibility boundaries

It owns no payment mechanism and holds no instrument data. It never defaults the acceptance
instant, under any policy, including the cascade that fills the line-level acceptance-due date.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-engine` — depends on
- `cpt-cf-bss-orders-lifecycle-component-workflow-seam` — shares model with

#### Workflow-seam handler

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-workflow-seam`

##### Why this component exists

R1 through R5 are only real if the operations the sibling gear calls are ordinary guarded
transitions rather than privileged state assertions.

##### Responsibility scope

The five workflow-only operations — approval reflection, begin fulfillment, spawn-signal report,
fulfillment acknowledgement, and workflow-mediated cancel with attached compensation evidence;
the recorded spawn signal that anchors the cancel guard; the persisted per-line line-to-subscription
linkage; the read-only per-line fulfillment projection; and the recorded deciding authority on
every stored verdict.

##### Responsibility boundaries

It implements no approval logic, no retry, no compensation and no provisioning. It never mirrors
the downstream `TransitionRequest` status, and it never derives order state from a stored
transition-request identifier.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-engine` — depends on
- `cpt-cf-bss-orders-lifecycle-component-preconditions` — shares model with

#### Hold-and-expiry handler

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-component-hold-and-expiry`

##### Why this component exists

An unbounded in-flight commercial state pins a price, holds an open promise to a customer and
accumulates operational debt — while an order whose subscriptions may already be provisioning
cannot be closed automatically.

##### Responsibility scope

Hold and resume with the stored pre-hold state; the per-state TTL policy for `submitted`,
`pending_approval`, `approved` and `on_hold`; the **resume cap** that stops a hold/resume cycle
restarting the dwell without limit; the coordinated expiry scheduler; and the transition-table
exclusion of `in_fulfillment` and of holds taken from it, together with the handoff of those cases
to the operational escalation owned by the sibling gear. It does **not** supply a fallback duration
for a state whose TTL is unset — such a state is unbounded, disclosed in
[`design/07-hold-and-expiry.md`](./design/07-hold-and-expiry.md) §4.2 and routed as
[`DECISIONS.md`](./DECISIONS.md) Q-27.

##### Responsibility boundaries

It does not pause entitlement or billing on already-activated subscriptions, does not extend a
term, and does not void wave-1 subscription drafts. It never auto-terminals an order whose
fulfillment may be in flight.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-engine` — depends on

#### Read-and-authorization handler

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-component-read-and-authz`

##### Why this component exists

Order consoles and downstream systems query state frequently against a strict read budget, and
cross-tenant leakage in a multi-tenant BSS gear is a critical confidentiality failure.

##### Responsibility scope

The current-version read projection and the paginated tenancy-scoped list with its state, date
and contract filters; historical version reads; the exposure of expected fulfillment time and
per-line deferral where the barrier deferred a line; audit-trail retrieval; and the per-actor
permission set including the cross-tenant delegation-proof requirement.

##### Responsibility boundaries

It is read-only and registers no transition. It never serves an order outside the caller's
tenancy scope, and it never exposes internal diagnostics through a read surface.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-engine` — depends on

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-interface-order-operations`

- **Requirement**: `cpt-cf-bss-orders-lifecycle-interface-order-ops` (PRD-defined business-operation set); the event contract `cpt-cf-bss-orders-lifecycle-contract-order-events` (PRD §9.2) is realised by [`design/01-foundation`](./design/01-foundation.md) §4.4
- **Technology**: REST/OpenAPI, registered through `OperationBuilder` with explicit response metadata, authentication flags and content-type policy
- **Location**: [`design/01-foundation`](./design/01-foundation.md) §3.3 is normative for request and response shapes, the concurrency token and the reason catalogue

The PRD specifies thirteen business operations without transport detail
(`cpt-cf-bss-orders-lifecycle-interface-order-ops`). This design binds them to one REST surface;
per-operation payloads and the machine-readable reason catalogue are owned by the slice that
raises each reason.

**Endpoints Overview** — the union of the seven slice surfaces plus the engine's one operator
surface, each owned by exactly one component:

| Method | Path | Owner | Stability |
|--------|------|-------|-----------|
| `POST` | `/bss-orders-lifecycle/v1/orders` | capture | unstable |
| `GET` | `/bss-orders-lifecycle/v1/orders` | read-and-authz | unstable |
| `GET` | `/bss-orders-lifecycle/v1/orders/{orderId}` | read-and-authz | unstable |
| `PATCH` | `/bss-orders-lifecycle/v1/orders/{orderId}` | capture | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/lines` | capture | unstable |
| `PATCH` | `/bss-orders-lifecycle/v1/orders/{orderId}/lines/{lineId}` | capture | unstable |
| `DELETE` | `/bss-orders-lifecycle/v1/orders/{orderId}/lines/{lineId}` | capture | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/preview` | gate-and-pin | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/submit` | gate-and-pin | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/amendments` | versioning | unstable |
| `GET` | `/bss-orders-lifecycle/v1/orders/{orderId}/versions` | read-and-authz | unstable |
| `GET` | `/bss-orders-lifecycle/v1/orders/{orderId}/versions/{version}` | read-and-authz | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/cancel` | hold-and-expiry | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/hold` | hold-and-expiry | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/resume` | hold-and-expiry | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/acceptance` | preconditions | unstable |
| `GET` | `/bss-orders-lifecycle/v1/orders/{orderId}/acceptance` | preconditions | unstable |
| `GET` | `/bss-orders-lifecycle/v1/orders/{orderId}/lines` | read-and-authz | unstable |
| `GET` | `/bss-orders-lifecycle/v1/orders/{orderId}/audit` | read-and-authz | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/approval-reflection` | workflow-seam | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/begin-fulfillment` | workflow-seam | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/spawn-signal` | workflow-seam | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/fulfillment-acknowledgement` | workflow-seam | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/workflow-cancel` | workflow-seam | unstable |
| `POST` | `/bss-orders-lifecycle/v1/outbox/dead-letters/{eventId}/re-drive` | foundation | unstable |

Twenty-five endpoints against the PRD's thirteen business operations, and the gap divides in two.
**Eleven** are surfaces the PRD describes in §6 without listing in §9.1 — line authoring
(three), the administrative edit, the acceptance write and read, the per-line read, the audit
read, the **version list** (§9.1's *Get order version* covers the single-version read only), the
spawn-signal report and the workflow-mediated cancel. **Ten of the eleven have an FR basis**; the
**audit read** does not — PRD §6.1 requires every transition to *be recorded* and
`nfr-order-audit-completeness` requires complete logging, but both are obligations on writing, not
on exposing, and §9.1 contains no audit-retrieval operation. It is grounded in a rationale — a
complete audit nobody can read is not an audit — rather than a requirement, and it exposes actor
identities, delegation-proof references and correlation identifiers, so it joins the re-drive as a
design-introduced surface needing Product's acknowledgement ([`DECISIONS.md`](./DECISIONS.md)
D-70). The other ten have an FR basis, so §9.1 needs a PRD amendment to remain the
normative operation set. **One** has no PRD basis at all: the operator outbox re-drive, introduced
by this design ([`DECISIONS.md`](./DECISIONS.md) D-17) because a parked dead-letter row **suspends its own order's event stream and no other** until re-drive republishes it in sequence order (D-87) — the re-drive may never be used to skip it — and because a parked row was
otherwise unrecoverable and the PRD's zero-silent-drops NFR could not be met without it. It is
recorded as a design-introduced operational surface rather than folded in as though the PRD asked
for it, and it is the second endpoint whose addition needs Product's acknowledgement rather than
merely §9.1's.

Every mutating operation requires an idempotency key, except the re-drive, which is idempotent by
construction because it republishes under the original event id. Submit, amendment, cancel and the
five workflow-only operations additionally require the optimistic version check, carried as an
ETag. State expiry and draft auto-void are scheduler-driven and deliberately absent from this
surface.

#### API evolution and stability

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-interface-api-evolution`

Two **stability zones** carry the gear's compatibility promise, both specified in
[`design/01-foundation`](./design/01-foundation.md) §4.6: the internal transition API and the
event contract. The REST surface carries per-endpoint stability, and every endpoint is `unstable`
today because no external consumer is in production.

**A change is breaking** if it removes or renames a state, an event type, a registered reason
name, a required envelope attribute, an endpoint or a required field; or if it narrows a value
set a caller may already send. Adding an optional field, a new reason, a new endpoint or a new
event payload member is **additive and non-breaking**.

**Promotion** from `unstable` to `stable` requires two consecutive releases with no breaking
change to that endpoint plus one external consumer in production. **Deprecation** of a `stable`
endpoint runs for two minor releases with the successor available throughout, announced in the
gear's changelog; a breaking change to a stability zone is a **major** version bump, and the
event contract is rolled out by publishing both majors until Workflow, Subscriptions and Billing
have migrated.

### 3.4 Internal Dependencies

| Dependency Gear | Interface Used | Purpose |
|-------------------|----------------|----------|
| `toolkit-db` | Runtime-scoped database access | Transactional persistence for version rows, the read projection, the audit store, the idempotency registry and the event outbox |
| Coordination lease library | SDK client | Singleton coordination for the per-state TTL expiry scheduler and the draft auto-void sweep |
| `orders-workflow` | SDK client and published events | Bidirectional seam: Workflow calls the transition operations and consumes the state events. This gear makes no outbound call to Workflow |

**Dependency Rules** (per project conventions):
- No circular dependencies
- Always use sdk modules for inter-gear communication
- No cross-category sideways deps except through contracts
- Only integration/adapter gears talk to external systems
- `SecurityContext` must be propagated across all in-process calls

### 3.5 External Dependencies

These are integration boundaries defined in [`PRD.md`](./PRD.md) §3.2 and §13, not components
owned here. Each is reached through a port so an unagreed or absent counterpart is a boundary
concern rather than a core change.

#### Pricing and catalog

| Dependency Gear | Interface Used | Purpose |
|-------------------|---------------|---------|
| `pricing` | SDK client | The published sellability predicates adopted by reference at submit, and the catalog price pin captured on every line |
| `rating` | SDK client | The price-evaluation contract producing the non-authoritative resolved total, **including the named TCV figure computed there rather than here**; composition system of record for the full pricing snapshot, which this gear never stores |
| Billing-chain tax owner | SDK client | The **indicative** tax figure Preview returns and never stores; the sixth outbound port, with its own unavailability reason |

#### Identity, contracts and downstream fulfillment

| Dependency Gear | Interface Used | Purpose |
|-------------------|---------------|---------|
| `account-management` | SDK client | Validation of the three tenant axes and party eligibility at submit |
| `contracts` | SDK client | Contract status and terms where a `contractId` is referenced; platform defaults govern where none is. Party-eligibility policy is unimplemented there |
| `subscriptions` | One **read-only** port, plus provisioning reached only through `orders-workflow` | Target of provisioning intents and system of record after spawn. This gear holds **no provisioning or state-mutating adapter** — that is the R3-relevant distinction — but `03` does hold a read-only overlap-presence port (`SUB-O5`, unagreed), which PRD §13 and §6.1 both anticipate. Fulfilment outcomes are learned only from Workflow acknowledgements |
| Generic Approval service | Reached only through `orders-workflow` | Owner of the approval-requirement verdict. Unspecified today; the verdict arrives as a stored fact with its decider recorded |
| Payments | Reached only through `orders-workflow` | Authorization outcome consumed as a begin-fulfillment guard input. No owning capability exists in the platform |

**Dependency Rules** (per project conventions):
- No circular dependencies
- Always use SDK modules for inter-gear communication
- No cross-category sideways deps except through contracts
- Only integration/adapter gears talk to external systems
- `SecurityContext` must be propagated across all in-process calls

### 3.6 Interactions & Sequences

Per-flow sequences are specified in the corresponding slice documents. The load-bearing ones:

#### Submit through the gate

**ID**: `cpt-cf-bss-orders-lifecycle-seq-submit-gate`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-new-acquisition`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-partner-admin`, `cpt-cf-bss-orders-lifecycle-actor-orders-catalog-pricing`, `cpt-cf-bss-orders-lifecycle-actor-orders-idp-ams`, `cpt-cf-bss-orders-lifecycle-actor-orders-contracts`

```mermaid
sequenceDiagram
    participant A as Partner Admin
    participant E as Transition Engine
    participant G as gate-and-pin
    participant P as pricing / rating
    A ->> E: submit (idempotency key, version)
    E ->> G: evaluate submit guard
    G ->> P: adopted predicates + pin + resolved total
    P -->> G: pass, pin, total
    G -->> E: guard satisfied, document contribution
    E ->> E: append version, audit, idempotency record, outbox entry
    E -->> A: submitted
```

**Description**: The pin and the total are captured inside the same transaction that commits
`submitted`, so a submitted line without a resolvable pin is not a state the store can hold. A
failing predicate refuses the whole order with a machine-readable reason and leaves it in
`draft`.

#### Amendment supersedes in-flight work

**ID**: `cpt-cf-bss-orders-lifecycle-seq-amendment-supersession`

**Mechanics specified in**: [`design/04-versioning`](./design/04-versioning.md) §3.6

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-amendment`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-partner-admin`, `cpt-cf-bss-orders-lifecycle-actor-orders-workflow`

```mermaid
sequenceDiagram
    participant A as Partner Admin
    participant E as Transition Engine
    participant W as Orders Workflow
    A ->> E: amend (idempotency key, version N)
    E ->> E: append version N+1, re-run gate, re-pin, audit
    E -->> A: version N+1 current
    E ->> W: OrderAmended (version N+1)
    W ->> E: reflect approval (version N)
    E -->> W: refused - stale version
```

**Description**: The version counter is the concurrency and supersession mechanism at once. Once
N+1 exists, any approval reflection or fulfillment acknowledgement carrying N is refused, which
is what makes asynchronous approval safe without distributed locking.

#### Fulfillment acknowledgement and linkage

**ID**: `cpt-cf-bss-orders-lifecycle-seq-fulfillment-acknowledgement`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-fulfillment-complete`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-workflow`, `cpt-cf-bss-orders-lifecycle-actor-orders-subscriptions`

```mermaid
sequenceDiagram
    participant W as Orders Workflow
    participant E as Transition Engine
    participant S as workflow-seam
    W ->> E: begin fulfillment (durably committed first)
    E -->> W: in_fulfillment - spawn signal window open
    W ->> E: acknowledge fulfillment (subscription ids, version)
    E ->> S: evaluate acknowledgement guard
    S -->> E: outcome, per-line linkage
    E ->> E: persist linkage, audit, outbox OrderCompleted
    E -->> W: completed
```

**Description**: Begin fulfillment commits before Workflow may issue any activation intent,
which establishes the cancel guard race-free. The order transitions to `fulfillment_failed`
only on an acknowledgement asserting that operational compensation completed.

#### Cancellation across the spawn boundary

**ID**: `cpt-cf-bss-orders-lifecycle-seq-cancel-guard`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-cancel-during-approval`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-seller-operator`, `cpt-cf-bss-orders-lifecycle-actor-orders-workflow`

```mermaid
sequenceDiagram
    participant O as Seller Operator
    participant E as Transition Engine
    participant W as Orders Workflow
    O ->> E: cancel (in_fulfillment, no spawn signal recorded)
    E -->> O: cancelled
    O ->> E: cancel (spawn signal recorded)
    E -->> O: refused - direct cancel window closed
    W ->> E: workflow-mediated cancel + compensation evidence
    E -->> W: cancelled
```

**Description**: The guard reads the recorded spawn signal, not the order state, so accepting a
wave-1 draft-create does not close the direct-cancel window. After `completed` there is no
order-side window at all.

#### Scheduler-driven expiry

**ID**: `cpt-cf-bss-orders-lifecycle-seq-state-expiry`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-new-acquisition`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-seller-operator`, `cpt-cf-bss-orders-lifecycle-actor-orders-workflow`

```mermaid
sequenceDiagram
    participant T as Expiry scheduler
    participant E as Transition Engine
    participant W as Orders Workflow
    T ->> E: expire eligible orders (per-state TTL)
    E ->> E: guard excludes in_fulfillment and holds taken from it
    E ->> E: append audit (actor - system), outbox OrderExpired
    E -->> T: expired
    E ->> W: OrderExpired - terminate process
```

**Description**: Expiry is an ordinary guarded transition with the system as actor. The
exclusions live in the transition table, so a scheduler defect cannot expire an order whose
subscriptions may be provisioning.

### 3.7 Database schemas & tables

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-db-orders-store`

**One ownership rule, stated here and in the foundation and nowhere else:** the **engine owns the
schema and is the sole writer** of every table below; a **slice owns the content** it contributes
and the guards that admit it. Column-level definitions, keys, constraints and indexes are
specified normatively in [`design/01-foundation`](./design/01-foundation.md) §3.7 for the
engine-owned tables, and in the introducing slice for the five it introduces. Resolved-total
columns are integer minor units at the currency's ISO 4217 scale. Immutability is declared **per
table** rather than globally, because ten of the nineteen are deliberately mutable.

| Table | Specified in | Content owner | Mutability |
|-------|--------------|---------------|------------|
| `orders_order` | `01 §3.7` | engine | mutable — denormalized state, pointers, counters |
| `orders_order_version` | `01 §3.7` | versioning | append-only |
| `orders_order_line_identity` | `01 §3.7` | capture | append-only |
| `orders_order_line` | `01 §3.7` | capture | append-only |
| `orders_draft_content` | `01 §3.7` | capture | **mutable** — the pre-submit working set |
| `orders_order_admin` | `01 §3.7` | capture | **mutable** — administrative content |
| `orders_order_line_admin` | `01 §3.7` | capture | **mutable** — administrative content |
| `orders_resolved_total` | `01 §3.7` | gate-and-pin | append-only |
| `orders_transition_audit` | `01 §3.7` | engine | append-only, hash-chained over committed entries; no UPDATE grant to any role, DELETE only to the retention worker for expired refused rows — `01 §3.7` is the canonical grant and retention contract |
| `orders_idempotency` | `01 §3.7` | engine | **mutable** — marker settles |
| `orders_event_outbox` | `01 §3.7` | engine | **mutable** — delivery bookkeeping; delivered rows purged |
| `orders_line_fulfillment` | `01 §3.7` | workflow-seam | **mutable** — projection advances |
| `orders_inflight_overlap_claim` | `01 §3.7` | gate-and-pin | **mutable** — only to set `released_at`; claims are never deleted |
| `orders_acceptance` | `01 §3.7` | preconditions | append-only |
| `orders_gate_outcome` | `03 §3.7` | gate-and-pin | append-only; Preview rows bounded retention |
| `orders_approval_reflection` | `06 §3.7` | workflow-seam | append-only |
| `orders_state_ttl_policy` | `07 §3.7` | hold-and-expiry | mutable policy rows |
| `orders_policy_election` | `05 §3.7` | preconditions | **mutable** — standing policy elections |
| `orders_read_access_log` | `08 §3.7` | read-and-authz | append-only |

There is no destructive path for any **order-linked commercial** row: an abandoned draft is
auto-voided to `expired` and remains readable. Four stores carry bounded retention by design, each executed by the retention sweep of §4.2 and each specified in its owning slice rather than here:
delivered outbox rows (30 days), Preview gate outcomes (7 days), refused-attempt audit rows
(90 days), and read-access-log rows (90 days).

**Migration and schema versioning.** Migrations are ordered by the phased slice map: the engine's
fourteen tables land in phase 0/1 before any slice, and each slice's own table lands with it.
Append-only tables need no backfill because a correction is a new row; the two mutable
administrative tables are additive. The gear exposes migrations and the runtime applies them, so
the schema version is the migration set the deployed gear carries, and a rollback is a
forward-only compensating migration rather than a down-migration.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-orders-lifecycle-topology-standard-bss-gear`

The gear runs as a stateless transition and read service over a shared `toolkit-db` backend,
with database privilege runtime-owned and the gear exposing migrations only. The audit role is
granted INSERT and SELECT only, which is half of what makes the trail tamper-evident.

**Six background workers** are lease-coordinated so a multi-replica deployment cannot double-act:
the **sharded outbox drain** — one lease per `order_id` hash shard, so event throughput scales
with replicas while per-order ordering holds — the per-state TTL expiry sweep, the draft auto-void
sweep, the idempotency-window sweep, the **retention purge sweep**, and the **audit-chain
verifier**, which walks orders in a rolling pass and is what makes §4.2's tamper-evidence claim
rest on an executor rather than on the chain alone (`design/01-foundation.md` §3.8). The outbox is
the only asynchronous egress.

The retention purge sweep exists because three of the four declared retention windows previously
had no executor: only delivered outbox rows were purged, by the drain. It runs singleton-leased on
a daily cadence with a bounded batch per store, and purges Preview gate-outcome rows past 7 days,
refused-attempt audit rows past 90 days, and read-access-log rows past 90 days. It holds the only
DELETE grant on the audit table and only for refused rows (`01 §3.7`). **A declared retention with
no worker behind it is an unbounded store**, and ADR-0005's cost argument depends on one of these
three actually running.

**Durability and recovery.** A committed transition is synchronously durable before
acknowledgement, so the write path is served from a primary with **synchronous commit to a quorum
including a standby in a second failure domain inside the residency boundary**, and never from an
asynchronously replicated primary. Recovery promotes that standby within the 60-minute RTO;
nightly base backups with continuous WAL archiving provide point-in-time recovery. A DR drill
runs each release. For residency-bound tenants every gear-owned store — tables, audit, outbox,
backups and the standby — is pinned in-jurisdiction with zero cross-boundary replication, which
is why the standby is a second failure domain rather than a second region. **The RPO-zero and
RTO-60-minute claims are therefore scoped to intra-cell failure domains** for a residency-bound
tenant: node and domain loss are covered, and loss of the whole jurisdictional cell has no
recovery path inside the boundary. That is accepted residual risk with the residency constraint as
its cause, and the DR drill's scope is stated to match rather than exercising a case the design
does not cover.

**Read path.** Reads are stateless and scale horizontally. Replica reads are **forbidden**: the
read projection is the aggregate row itself, so there is no lag to tolerate and a lagging replica
would answer successfully with stale state that nothing detects.

**Infrastructure as code** is platform-owned: provisioning, environment parity, auto-scaling
configuration and resource tagging are inherited from the platform's deployment tooling and this
gear declares no infrastructure of its own. The deliberately unchosen policy values of
[`design/07-hold-and-expiry`](./design/07-hold-and-expiry.md) §4.5 and
[`design/08-read-and-authz`](./design/08-read-and-authz.md) §4.5 are delivered as
`orders_state_ttl_policy` rows and gear configuration, promoted through environments with the
deployment rather than edited at runtime.

**Health reporting** distinguishes readiness from liveness: store unavailability makes the
instance **not ready**, so it stops receiving traffic while remaining alive, rather than being
killed and restarted into the same unavailable store.

## 4. Additional context

### 4.1 Capacity and cost

Working baselines pending the program-wide NFR workshop, recorded as numbers rather than left
blank because a threshold nobody set is a threshold nobody can verify against
([`DECISIONS.md`](./DECISIONS.md) D-41).

| Dimension | Baseline | Note |
|-----------|----------|------|
| Peak order transitions | **50 / second** | The load the p95 < 1 s commit budget is asserted at |
| Outbox drain throughput | **200 events / second** | Across all shards; must exceed the transition rate because one transition can emit one event and re-drives add load |
| Event-delivery budget | **30 s p95** from commit to bus | The target drain lag is verified and alerted against; matches the sibling gear's process-event latency class |
| Row growth | **~12 rows** per order at version 1, **~6** per amendment | Aggregate, identity, lines, totals, audit, outbox |
| Archival tier trigger | **24 months** past a terminal state | The append-only model permits it because nothing reads a terminal order's version chain on a hot path |
| List page size | default **50**, maximum **200** | The 200 ms read budget is per page, so an unbounded page would make it meaningless |

Cost is dominated by the shared `toolkit-db` backend and scales with retained order history. The
gear is sized by transition rate rather than data volume: an order is a handful of small rows and
the version chain grows only on amendment, which is rare relative to submit. Read load is absorbed
by the aggregate row rather than the write path. The six background workers are lease-coordinated
and idle-cheap.

### 4.2 Security posture

**Authentication** is platform-owned: the inbound gateway terminates OAuth 2.0 and the gear
receives an authenticated `SecurityContext` propagated across every in-process call, never
re-implementing token handling. **Service identity** is separate and explicit: the five
workflow-only operations require a **gateway-asserted service principal plus a scope claim naming
this gear**, checked by the pre-guard — actor class alone is insufficient, because on its own
nothing would distinguish the sibling gear from any caller presenting that class.

**Authorization** is deny-by-default, evaluated by **one** authorization evaluator invoked by both
the engine pre-guard and the read paths, so there is a single model rather than two that drift.
Cross-tenant action requires a **verifiable delegation proof** — a signed assertion from Account
Management naming the delegating tenant, the delegated scope, the delegate, an issue instant and a
finite expiry, verified against a published issuer key and revocable by the delegating tenant,
aligned with BSS manifest §2.1.3. Its reference is recorded on the audit entry. Absence, expiry or
revocation is a refusal.

The gear stores no cardholder data and holds no payment instrument, so PCI DSS is **not
applicable**; it consumes an authorization *outcome* only.

#### Threat model

| Threat | Vector | Boundary crossed | Mitigation | Residual risk |
|--------|--------|------------------|------------|---------------|
| Cross-tenant order disclosure | A caller reads or lists an order outside their relationship | Tenant boundary | Scope by relationship not tenant equality; not-found rather than forbidden; delegation proof required and audited | A compromised delegation credential reads within its granted scope until revoked |
| A partner manufactures customer consent | The placing party records the acceptance instant themselves | Commercial-evidence boundary | The placing actor is normatively barred from recording acceptance for the order they placed | An offline collusion between partner and a customer principal is out of scope for a technical control |
| State asserted without a guard | A caller reaches a state-setting path directly | Engine boundary | There is no such path: every state change is a guarded transition and the engine is sole writer | A privileged database credential bypasses the engine; mitigated by runtime-owned privilege and the audit hash chain making it detectable |
| Sibling-gear impersonation | Any caller presents the Workflow actor class | Service boundary | Gateway-asserted service principal plus a gear-scoped claim | A compromised platform gateway; out of this gear's control |
| Audit tampering | A holder of database privilege edits or deletes trail rows | Data boundary | No UPDATE or DELETE grant on the audit role, plus a per-order predecessor-hash chain verified periodically | A holder of the migration role can drop the grant; detectable via the chain and the grant audit |
| Preview amplification | Unauthenticated-shaped basket calls fan out to six ports and write outcome rows | Cost and dependency boundary | Preview declares its actor classes, carries a rate limit, and its outcome rows have a bounded retention | A high-volume authorised caller can still consume port capacity, bounded by the per-port bulkhead |
| Unbounded audit growth | Repeated refused attempts against one order | Availability boundary | Refusal rows carry 90-day retention and repeated refusals are rate-limited | A distributed low-rate refusal campaign remains possible and is a monitoring concern |
| An order held in-flight indefinitely | An actor with hold permission cycles hold and resume before each TTL elapses, restarting the dwell | Commercial-promise boundary | The resume cap of [`design/07-hold-and-expiry.md`](./design/07-hold-and-expiry.md) §4.2 — a counter only the resume transition increments and no transition resets — bounds total in-flight life at `(cap + 1) × TTL` | Where the state's TTL is **unset** the cap bounds nothing, because the dwell it multiplies is itself unbounded; disclosed as [`DECISIONS.md`](./DECISIONS.md) Q-27 and alerted per `07 §3.8` |

### 4.3 Data protection, residency and retention

**Encryption**: at rest by the platform storage layer, TLS in transit on every hop including the
six outbound ports and the event bus. **Key management** is the platform KMS; the gear holds no
key material.

**Classification**: order content and its resolved totals are **commercial-confidential**; actor
and tenant identifiers in the audit trail are **personal-minimal**; the free-text administrative
fields — display labels and internal notes — are **personal-minimal** and carry length bounds and
input validation. No masking requirement arises, because no surface returns another tenant's data.

**Erasure** is the one case that touches the immutable stores. Because they are append-only, an
erasure obligation is satisfied by **pseudonymising actor identifiers in place** — the single
permitted mutation of the audit store, itself performed as an audited transition and recorded, so
the hash chain is re-derived rather than broken. Commercial content is not erased, because it is a
financial record retained under the program retention policy.

**Residency**: for residency-bound tenants every gear-owned store — tables, read projection,
audit, idempotency registry, outbox, backups and the synchronous standby — is pinned to an
in-jurisdiction deployment cell with zero cross-boundary replication (see
`cpt-cf-bss-orders-lifecycle-constraint-data-residency`).

**Retention**: append-only with no destructive path for any `submitted`-or-beyond order; an
abandoned draft is auto-voided to `expired` and remains readable. Four stores carry bounded
retention by design: delivered outbox rows (30 days), Preview gate outcomes (7 days),
refused-attempt audit rows (90 days), and read-access-log rows (90 days). The commercial retention period itself is a PRD open
question ([`DECISIONS.md`](./DECISIONS.md) Q-07).

### 4.4 Observability

Signals are owned **per slice**, each declaring its own metrics, log fields and alerts in its
`§3.8`; the engine's own are in [`design/01-foundation`](./design/01-foundation.md) §3.8. The
`correlationId` supplied by the sibling gear is recorded on the audit row and propagated to logs
and to every outbound port call, which is what makes an order's whole approval-to-fulfillment arc
traceable across two gears — propagation onward through Subscriptions is the unagreed `SUB-O9`
ask.

**Alerting** covers both the invariant-bearing signals and the latency SLOs: commit-latency and
read-latency SLO burn-rate alerts derived from the p95 budgets, drain lag beyond the 30-second
event-delivery budget, any audit-append failure, any audit-chain verification mismatch, any
non-zero unaudited-transition count, dead-letter arrivals, and orders held in `in_fulfillment`
past the overdue window. Health reporting distinguishes readiness from liveness (§3.8).

### 4.5 Error handling and the outbox failure posture

Errors classify three ways. A **guard refusal** is an expected business outcome carrying a stable
machine-readable reason, mapped to an RFC 9457 problem with no internal diagnostics on the wire —
and one name per condition, since callers key on the string. A **concurrency refusal** — payload
mismatch, still-processing, version conflict — is retryable under the rules in
[`design/01-foundation`](./design/01-foundation.md) §4.2 and must never be read as success. An
**infrastructure fault** aborts the transaction, so a failed audit or outbox append leaves no
state change behind.

Outbound port failures are bounded rather than merely reported: **per-port deadlines** inside a
total request budget, bounded retry on transient failure only, a **circuit breaker** per port
mapping to that port's existing fail-closed reason, and a **concurrency bulkhead** per port, all
specified in [`design/03-gate-and-pin`](./design/03-gate-and-pin.md) §2.2.

Outbox delivery is at-least-once with consumer de-duplication on event ID; a repeatedly failing
entry is parked in an inspectable dead-letter record with an alert, and is recoverable by the
**operator re-drive** which republishes under the original event id. A parked event is never an
order state, and the order's own state is already committed and correct. Compensating-transaction
patterns are deliberately absent: this gear holds no distributed saga, and every failure it can
suffer is contained in one database transaction.

### 4.6 Testability

Three test classes are conditions of the guarantees this design claims. The state machine is a
declarative table, so **edge coverage is enumerable**: a structural test asserts every row writes
an audit row on both outcomes and that no edge exists outside the table. Idempotency and the
version check are concurrency properties, verified by **parallel same-key execution** asserting
one durable effect. A crash test asserting a lease-expired marker is recoverable is **planned and not yet written** — this gear has no implementation and no runtime tests, so a claim that one exists would be false. The
unagreed downstream seams are behind ports, so the gate and acknowledgement paths are **testable
against a contract double** before Subscriptions exists. The sibling gears' precedent of
jointly-owned golden fixtures before implementation applies to the gate's adopted predicates,
since forking them silently is exactly what a shared fixture catches.

### 4.7 Extension and provenance

**Extension points** are normative in [`design/01-foundation`](./design/01-foundation.md) §4.6:
a slice may add a guard, a reason, a document contribution, a policy row or a table of its own
without touching the engine; a state, a transition row, an event type, an envelope attribute or
an engine-owned column requires an engine change, and adding a state or event type is
additionally a PRD question because both sets are enumerated there.

**Decisions** are recorded in [`DECISIONS.md`](./DECISIONS.md) — ninety entries plus **thirty**
routed open questions — with **seven** ADRs in [`ADR/`](./ADR/) carrying full alternatives
analysis. **Upstream asks** are declared in [`UPSTREAM_REQS.md`](./UPSTREAM_REQS.md),
including `SUB-O10`, which this design raises.

**PRD open questions.** Of the fifteen rows in [`PRD.md`](./PRD.md) §15, twelve are unanswered.
Two are resolved by this design and cite their row where they resolve it: the pre-subscription
resolved-total scope ([`design/03-gate-and-pin`](./design/03-gate-and-pin.md) §4.5) and the
missing-required-date behaviour ([`design/02-capture`](./design/02-capture.md) §4.2). Three that
this design previously passed over in silence are now explicit deferrals with their PRD owners —
trial-conversion and renewal classification, subscription composition granularity, and the
quantity model — recorded as Q-01, Q-02 and Q-03. **Four of the remaining seven carry design
interim positions**: rows 5 and 7 (retention and per-state TTLs) as Q-07 and Q-06, row 12 (the
overlap-key dimension) as Q-05, and row 3 (the order reference on `create`) as upstream ask
`SUB-O2`. Rows 2, 14 and 15 are genuinely unaffected by this
design.

**Deferred to a later phase by the PRD**, and therefore absent by decision rather than omission:
the `change` order category — **modeled and refused this phase**, with the enum left open to a
third value pending Q-01 — add-on selection on the line, commercial bounds on a usage-bearing
line, partial fulfillment with per-line terminals, deal and quote provenance references, and any
CPQ or quote artifact.

**What that deferral means commercially, stated so the phase is not over-read.** `new_sale` means
**every line spawns a new subscription**, so this gear covers **net-new acquisition only**. An
existing customer adding seats, upgrading a plan, or changing term is a *commercially initiated
change* — `category = change` — and is refused at creation
([`design/02-capture.md`](./design/02-capture.md) §2.2). Those motions continue down the
pre-existing direct-subscription path, which means that for the whole of this phase the expansion
half of a subscription business has **no order document, no sellability gate at the point of
change, no price pin, no approval arc and none of the audit trail this gear exists to provide**.
The gap is the PRD's declared phasing and not a design defect; it is recorded here because "Orders
is live" and "commercial changes are governed by Orders" are different claims, and only the first
becomes true at the end of this phase.

**Sibling-gear evidence base.** The engine-shaped core, the append-only history with in-table
supersession, the transactional outbox, the integer-minor-unit money convention, the
read-projection-for-latency pattern and lease-coordinated background work are all adopted from the
two built BSS gears rather than invented here.

## 5. Traceability

- **PRD**: [`PRD.md`](./PRD.md)
- **ADRs**: [`ADR/0001`](./ADR/0001-cpt-cf-bss-orders-lifecycle-adr-transition-through-engine.md) transition through the engine; [`ADR/0002`](./ADR/0002-cpt-cf-bss-orders-lifecycle-adr-slice-decomposition.md) the foundation-plus-seven-slices decomposition; [`ADR/0003`](./ADR/0003-cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate.md) fail closed on an unevaluable gate input; [`ADR/0004`](./ADR/0004-cpt-cf-bss-orders-lifecycle-adr-closed-enumerations.md) both enumerations stay closed; [`ADR/0005`](./ADR/0005-cpt-cf-bss-orders-lifecycle-adr-refusals-commit.md) a refused transition is a committed outcome; [`ADR/0006`](./ADR/0006-cpt-cf-bss-orders-lifecycle-adr-outbox-publication.md) events publish asynchronously from an outbox; [`ADR/0007`](./ADR/0007-cpt-cf-bss-orders-lifecycle-adr-in-transaction-concurrency.md) concurrency rules are in-transaction constraints. All seven `accepted`, all seven cited by ID in §1.2
- **Design set**: [`design/`](./design/) — the Transition Engine plus per-capability slice designs; the phased map and dependency order are in [`design/README.md`](./design/README.md), which is the build-order authority.
- **Sibling gear**: [`../../orders-workflow/docs/PRD.md`](../../orders-workflow/docs/PRD.md) — process orchestration; the seam rules R1–R5 are normatively owned by [`PRD.md`](./PRD.md) §6.4 and are not restated here.
