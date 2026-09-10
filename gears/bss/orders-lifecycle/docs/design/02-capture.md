<!-- CONFLUENCE_TITLE: [BSS]: Orders Lifecycle — Draft Capture and the Line Model (Slice 2) -->
<!-- Related: ../DESIGN.md, ../PRD.md, ./README.md, ./01-foundation.md | Owners: BSS Orders team -->

# DESIGN — Draft Capture and the Line Model (Slice 2)


<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Principles and Constraints](#2-principles-and-constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Domain Model](#31-domain-model)
  - [3.2 Component Model](#32-component-model)
  - [3.3 API Contracts](#33-api-contracts)
  - [3.4 Internal Dependencies](#34-internal-dependencies)
  - [3.5 External Dependencies](#35-external-dependencies)
  - [3.6 Interactions and Sequences](#36-interactions-and-sequences)
  - [3.7 Database Schemas and Tables](#37-database-schemas-and-tables)
  - [3.8 Deployment Topology](#38-deployment-topology)
- [4. Additional Context](#4-additional-context)
  - [4.1 What a draft may and may not hold (normative)](#41-what-a-draft-may-and-may-not-hold-normative)
  - [4.2 The date cascade (normative)](#42-the-date-cascade-normative)
  - [4.3 Field classification (normative)](#43-field-classification-normative)
  - [4.4 Line shapes that are one line (normative)](#44-line-shapes-that-are-one-line-normative)
  - [4.5 Draft abandonment](#45-draft-abandonment)
- [5. Traceability](#5-traceability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-design-capture`

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice owns everything an order carries **before** anything is validated: the aggregate's
identity and parties, the line model, the three line dates and their cascade, the quoted
commercial shape of the deal, and the field-level classification that later makes commercial
content immutable while administrative content stays editable
([`../PRD.md`](../PRD.md) §6.1).

Its governing choice is that a `draft` is **deliberately unvalidated**. No sellability predicate
runs, no catalog price pin is captured, no total is resolved. A basket must be assemblable at
the cost of a row insert, because a partner building a five-line order should not pay five
catalog round-trips per keystroke, and because the pre-submit arc is exactly where a buyer is
still deciding. Validation is one event — submit — and it belongs to
[`03-gate-and-pin`](./03-gate-and-pin.md).

The second choice is subtler and shapes every later slice: **lines belong to a version, not to
the order**. A line carries a stable `line_id` that survives amendment, so "the same line, at
quantity 25 instead of 10" is expressible and traceable, while the row itself is immutable once
its version is superseded. This is what lets [`04-versioning`](./04-versioning.md) append rather
than mutate, and it is why the per-line fulfillment projection can key on `line_id` without
caring which version won.

The slice authors nothing about money beyond persistence. It records `priceId` as an opaque
reference and never derives an amount, per the boundary rule that keeps all price math in the
price-evaluation domain.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|------------------|
| `cpt-cf-bss-orders-lifecycle-fr-order-create` | Creation is a `draft` transition through the engine with a single admission guard: category admissibility. Identity and the human-readable number are assigned in the same transaction, so a created order is always addressable both ways. |
| `cpt-cf-bss-orders-lifecycle-fr-order-line-dates` | The three dates are line columns with a declared cascade (§4.2) resolved once at submit by the gate, never at authoring or read time, so two readers cannot disagree about an effective date. Term duration and billing cycle are captured as authored values. |
| `cpt-cf-bss-orders-lifecycle-fr-order-tenant-axes` | The three axes and the initiating actor are authored at creation and carried on the aggregate. This slice writes them; freezing them is the submit guard's job. |
| `cpt-cf-bss-orders-lifecycle-fr-order-amendment` | The commercial-versus-administrative field classification is declared here as data (§4.3) and consumed by the versioning slice, so the immutability split is enforced from one table rather than by reviewer discipline. |
| `cpt-cf-bss-orders-lifecycle-fr-order-submit` | The single-currency basket rule is authored as an admission guard on line insert, so a mixed-currency basket is refused at the point of the mistake rather than surviving to submit. |

#### NFR Allocation

| NFR ID | NFR Summary | Allocated To | Design Response | Verification Approach |
|--------|-------------|--------------|-----------------|----------------------|
| `cpt-cf-bss-orders-lifecycle-nfr-order-transition-latency` | Transition commit p95 < 1 s | Line-model authoring | Draft authoring resolves no external input, so its guard set is local-only and the commit path is a single insert or update | Load test on draft create and line insert at basket sizes up to the declared line cap |
| `cpt-cf-bss-orders-lifecycle-nfr-order-retention` | Abandoned drafts auto-voided | Draft abandonment | Every draft carries its creation instant, which is the only input the auto-void sweep in `07` needs; auto-void is a state transition, never a delete | Test asserting an auto-voided draft remains readable and its audit trail intact |
| `cpt-cf-bss-orders-lifecycle-nfr-order-audit-completeness` | 100 % of amendments audited | Field classifier | Administrative edits are engine transitions like any other, so they audit without a version bump | Test asserting an administrative edit produces an audit row and no new version |

#### Key ADRs

The seven gear ADRs govern this slice, chiefly [`ADR/0001`](../ADR/0001-cpt-cf-bss-orders-lifecycle-adr-transition-through-engine.md)
and [`ADR/0002`](../ADR/0002-cpt-cf-bss-orders-lifecycle-adr-slice-decomposition.md). One decision
taken in this slice carries its own register entry: **a missing required line date is refused at
the gate rather than held in a waiting state** (§4.2), whose rejected alternative was a twelfth
order state. Recorded as [`../DECISIONS.md`](../DECISIONS.md) D-60, resolving **PRD §15 row 8**,
whose owner is Product with Design; its full alternatives analysis is consolidated into
[`ADR/0004`](../ADR/0004-cpt-cf-bss-orders-lifecycle-adr-closed-enumerations.md) alongside D-14,
D-15 and D-16, and §4.2 is one of that ADR's normative design homes.

### 1.3 Architecture Layers

Layering is inherited from [`01-foundation`](./01-foundation.md) §1.3 unchanged. This slice
contributes guard predicates and document contributions at the application layer and adds no
infrastructure of its own.

## 2. Principles and Constraints

### 2.1 Design Principles

#### A draft is unvalidated by construction

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-draft-is-unvalidated`

No sellability predicate, catalog resolution or price evaluation runs while an order is in
`draft`. The only guards on authoring are local and structural: category admissibility, currency
consistency, and referential shape. A consequence worth stating plainly is that a `draft` may
hold references that no longer resolve — a retired plan, a withdrawn price — and that is
correct: the gate is where that becomes a refusal, and discovering it earlier would mean paying
for validation on every keystroke.

#### Line identity is stable across versions

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-line-identity-stable`

A line's `line_id` is assigned once and reused by every later version that carries that line.
Version rows are immutable, but the identity is not version-scoped. Without this, an amendment
that changes a quantity would be indistinguishable from one that removed a line and added
another, and the per-line fulfillment projection would have nothing durable to key on.

#### Field class is declared, not inferred

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-field-class-declared`

Every authored field is declared in exactly one class in one table (§4.3) — **commercial**,
**commercial-frozen** or **administrative**, with read-through fields named as never authored
([`../DECISIONS.md`](../DECISIONS.md) D-62). The
versioning slice reads that declaration; it does not maintain its own list. A new field is
unusable until it is classified, which is deliberate — an unclassified field would silently
become editable after `submitted`, quietly breaking the commercial audit trail the gear exists
to provide.

### 2.2 Constraints

#### The basket is single-currency

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-single-currency-basket`

All lines of an order share one currency, enforced on line insert rather than only at submit.
This is an MVP basket constraint carried from the PRD, not a platform limit: the downstream
chain binds one currency per invoice, so a mixed basket has no coherent resolved total and no
coherent market. Lines **may** differ in billing frequency — each spawned subscription owns its
own cycle.

#### The `change` category is refused

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-change-category-refused`

`category` is authored as `new_sale` or `change`, and `change` is **refused at creation** with
its own machine-readable reason until the change-order path ships. The field exists in the model
now because the state machine, the event set and the line model all depend on whether a line may
target an existing subscription; admitting the value before that path exists would produce
orders nothing can fulfil.

#### One order, one payer

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-single-payer`

All lines share the order's three tenant axes and in particular a single `payerTenantId`. A
buyer purchasing for two different payers authors two orders. The axes are authored here and
frozen by the submit guard; only `payerTenantId` has an amendment path, owned by
[`04-versioning`](./04-versioning.md).

#### Add-on selection is not expressible

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-no-addon-selection`

The line carries `skuId`, `planId`, `priceId`, `qty` and the pin, and does **not** express which
plan-scoped add-ons a buyer selected. Add-on rules are authored in the pricing gear and compose
on the subscription downstream. The consequence is stated rather than hidden: a plan carrying a
**required** add-on is not orderable as one commercial intent this phase, and the submit gate
therefore evaluates no add-on bounds.

## 3. Technical Architecture

### 3.1 Domain Model

**Core Entities**: this slice authors the entities the engine persists; it introduces one of its
own.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-line-date-set`

The resolved triple of contract-effective date, service-activation date and acceptance-due date
for one line, together with the policy switch state that governed whether the latter two were
required. Authored values are retained in draft; **resolution happens at submit**, performed by
the gate and materialised with the admitted version (§4.2), so no reader re-derives a cascade and
no two components can resolve it differently.

It contributes to `cpt-cf-bss-orders-lifecycle-entity-order-root` (identity, number, category,
axes, initiating actor, contract reference) and owns the content of
`cpt-cf-bss-orders-lifecycle-entity-order-version-chain` lines as defined in
[`01-foundation`](./01-foundation.md) §3.1.

**Relationships**:
- `Order line` → `Line date set`: one-to-one, embedded in the line row rather than a separate table, because the triple has no independent lifecycle.
- `Order line` → `Order version`: many-to-one; the line's `line_id` is stable while the row is version-scoped and immutable.

### 3.2 Component Model

This slice realises `cpt-cf-bss-orders-lifecycle-component-capture`
([`../DESIGN.md`](../DESIGN.md) §3.2) as two internal parts.

#### Line-model authoring

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-capture-line-model`

##### Why this component exists

The line is where the quoted commercial shape of the deal is either captured or lost. Term and
cycle in particular have no other home: without them a one-year commitment and a monthly rolling
deal are indistinguishable downstream.

##### Responsibility scope

Order creation and identity assignment; the human-readable number; line insert, update and
removal while in `draft`; `line_id` assignment and its stability contract; the date cascade;
term duration and billing cycle; external references; the single-currency and single-payer
guards; bundle and one-time-plan line handling.

##### Responsibility boundaries

It resolves no catalog reference, captures no pin, computes no total, and evaluates no
sellability predicate. It does not decide whether a missing optional date blocks submit — it
records the policy switch state that the gate reads.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-orchestrator` — depends on
- `cpt-cf-bss-orders-lifecycle-component-gate-and-pin` — shares model with
- `cpt-cf-bss-orders-lifecycle-component-capture-field-classifier` — depends on

#### Field classifier

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-capture-field-classifier`

##### Why this component exists

The commercial-versus-administrative split is the mechanism behind the whole versioning
contract, and it is only trustworthy if it lives in one declaration that both the capture path
and the amendment path read.

##### Responsibility scope

The declaration table of every authored field with its class (§4.3); the admission check that
rejects an edit touching a commercial field outside `draft`; and the startup check that fails if
any authored field is unclassified.

##### Responsibility boundaries

It holds no field values and performs no edit. It does not decide what an amendment does with a
commercial change — that is [`04-versioning`](./04-versioning.md).

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-versioning` — owns data for

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-interface-capture-ops`

- **Requirement**: `cpt-cf-bss-orders-lifecycle-interface-order-ops`
- **Technology**: REST/OpenAPI registered through `OperationBuilder` with explicit response metadata; RFC 9457 problems

| Method | Path | Description | Stability |
|--------|------|-------------|-----------|
| `POST` | `/bss-orders-lifecycle/v1/orders` | Create an order in `draft` with its axes, category and optional contract reference | unstable |
| `PATCH` | `/bss-orders-lifecycle/v1/orders/{orderId}` | Edit administrative content; refused for commercial fields outside `draft` | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/lines` | Add a line while in `draft` | unstable |
| `PATCH` | `/bss-orders-lifecycle/v1/orders/{orderId}/lines/{lineId}` | Amend a line while in `draft` | unstable |
| `DELETE` | `/bss-orders-lifecycle/v1/orders/{orderId}/lines/{lineId}` | Remove a line while in `draft` | unstable |

**Reasons contributed to the registry**: category-not-admitted, currency-mismatch,
payer-mismatch, line-not-in-draft, commercial-field-immutable, field-unclassified,
line-cap-exceeded, date-cascade-invalid (defined once here with the cascade it governs, and
raised by the gate — [`03-gate-and-pin`](./03-gate-and-pin.md) §3.6 *Run Gate and Submit* step 9 —
since submit is where the cascade resolves).

**Order number**: assigned at creation, unique per `sellerTenantId`, and treated as a display
and reconciliation handle only — no logic keys on its structure, so its format may change under
the additive-change policy without a major version.

### 3.4 Internal Dependencies

| Dependency Gear | Interface Used | Purpose |
|-------------------|----------------|----------|
| `toolkit-db` | Runtime-scoped database access, via the engine | Line and aggregate persistence inside the transition transaction |

No other internal dependency. The slice makes no call outside the gear.

### 3.5 External Dependencies

None. This slice deliberately resolves no external reference — that property is what makes a
`draft` cheap, and it is why an unreachable catalog cannot block basket assembly.

### 3.6 Interactions and Sequences

#### Create draft order

**ID**: `cpt-cf-bss-orders-lifecycle-seq-create-draft`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-new-acquisition`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-partner-admin`, `cpt-cf-bss-orders-lifecycle-actor-orders-direct-customer`, `cpt-cf-bss-orders-lifecycle-actor-orders-seller-operator`

**Algorithm: Create Draft Order**

Input: category, tenant_axes, initiating_actor, contract_id, security_context, idempotency_key
Output: order identity and number, or a registered refusal

1. [ ] - `p1` - Declare the category-admissibility guard so the engine evaluates it and audits its refusal - `inst-cd-declare-category-guard`
2. [ ] - `p1` - Assign a new order identity - `inst-cd-assign-identity`
3. [ ] - `p1` - Assign the human-readable order number, unique within seller_tenant_id - `inst-cd-assign-number`
4. [ ] - `p1` - Record the three tenant axes and the initiating actor - `inst-cd-record-axes`
5. [ ] - `p1` - **IF** a contract reference was supplied: record it without resolving it - `inst-cd-record-contract-ref`
6. [ ] - `p1` - Request the create transition from the engine with the contribution - `inst-cd-request-transition`
7. [ ] - `p1` - **RETURN** identity and number - `inst-cd-return-identity`

**Description**: Nothing here reaches outside the gear. The contract reference is recorded, not
resolved — resolution is a submit-gate predicate, so a contract that goes inactive between
capture and submit is caught where it matters.

#### Author a line and resolve its dates

**ID**: `cpt-cf-bss-orders-lifecycle-seq-author-line`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-new-acquisition`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-partner-admin`

**Algorithm: Author Line**

Input: order_id, sku_id, plan_id, price_id, qty, currency, dates, term_duration, billing_cycle
Output: line_id, or a registered refusal

1. [ ] - `p1` - Declare three guards the engine evaluates and audits: order-in-draft, currency-consistency against the order's other lines, and the line cap - `inst-al-declare-guards`
2. [ ] - `p1` - Assign a stable line_id - `inst-al-assign-line-id`
3. [ ] - `p1` - Retain any explicitly authored calendar field as authored; resolution of the cascade belongs to the gate per §4.2 and is **not** performed here - `inst-al-retain-authored-dates`
4. [ ] - `p1` - Record term_duration and billing_cycle as authored - `inst-al-record-term-cycle`
5. [ ] - `p1` - Record the optional external reference - `inst-al-record-external-ref`
6. [ ] - `p1` - Request the line-authoring transition from the engine - `inst-al-request-transition`
7. [ ] - `p1` - **RETURN** line_id - `inst-al-return-line-id`

**Description**: Explicit values are retained in draft; the cascade is resolved once at submit
and stored with the admitted version. A later reader never re-derives it, which is what stops two
surfaces disagreeing about an effective date after a policy switch changes.

### 3.7 Database Schemas and Tables

This slice introduces no table. It owns the content of `orders_order_line`
(`cpt-cf-bss-orders-lifecycle-dbtable-order-line`) and contributes columns to `orders_order`,
both specified normatively in [`01-foundation`](./01-foundation.md) §3.7. Two additions to that
specification belong here:

- Order-scoped line identity is a **real key**: `orders_order_line_identity(order_id, line_id)` is the parent every version-scoped line row, per-line total and per-line projection references. The line row's own key remains `(order_id, version, line_id)`, so the row is version-scoped while the identity is not — and the uniqueness the projection depends on is enforced rather than asserted.
- `orders_order_line` carries the **resolved** date triple plus the policy-switch state that governed it, so the resolution is auditable after the switch changes.

The declared line cap — a working baseline of **200 lines**, chosen so a capped basket's catalog
resolution stays inside the 250 ms port deadline — and the order-number format are configuration
rather than schema, held
with the slice's other tenant-scoped policy values.

### 3.8 Deployment Topology

Inherited from [`01-foundation`](./01-foundation.md) §3.8 unchanged. This slice adds no
background worker.

**Observability owned here**: draft age distribution and the count of drafts approaching the
auto-void TTL, because a basket abandoned near the boundary is the case the sweep will act on;
line-mutation rate per order, which is what distinguishes normal authoring from a client retry
loop; field-classification refusals split by field, since a rising rate means the classifier and
the caller disagree about what is commercial; and the count of lines whose required dates are
absent at submit, the refusal §4.2 owns. Alerts fire on classification refusals crossing their
threshold and on draft creation succeeding while line insertion fails, which indicates a partial
client flow rather than a gear fault.

## 4. Additional Context

### 4.1 What a draft may and may not hold (normative)

A `draft` order **MUST** be modifiable freely: lines added, amended and removed, administrative
and commercial content alike edited in place. This is possible because draft content lives in the
**mutable** `orders_draft_content` table rather than in the append-only version chain, and the
submit transition materialises it into version 2 ([`01-foundation`](./01-foundation.md) §3.7).
Without that separation, "a draft is freely modifiable" and "the version chain is append-only"
would contradict each other. Authoring **MUST NOT** resolve a catalog
reference, evaluate a sellability predicate, capture a price pin or produce a resolved total. A
`draft` **MAY** therefore hold references that no longer resolve, and that state **MUST NOT** be
treated as an error until submit.

An order **MUST** carry at least one line at submit; the gate enforces it. A `draft` with zero
lines is valid and expected — it is the state an order exists in between creation and the first
line.

### 4.2 The date cascade (normative)

Each line carries three calendar fields. Their relationship is fixed:

| Field | Requirement | Default |
|-------|-------------|---------|
| Contract-effective date | Mandatory at submit | The **submit instant**, if unauthored |
| Service-activation date | Optional unless the tenant policy switch requires it | The contract-effective date |
| Customer-acceptance due date | Optional unless the tenant policy switch requires it | The contract-effective date |

An explicitly authored date is retained in draft. At submit, the gate **MUST** resolve the
cascade: where the contract-effective date remains unauthored, it **MUST** use the submit commit
instant, then derive any unauthored dependent dates from that resolved value. The resolved
contract-effective, service-activation and acceptance-due dates **MUST** be stored respectively in
`orders_order_line.contract_effective_date`, `.service_activation_date` and
`.acceptance_due_date`; only the tenant policy-switch state that governed the cascade belongs in
`orders_order_line.date_policy_switch_state`. Resolution **MUST NOT** be deferred to read time or
to fulfillment; it is a submit contribution materialised with the admitted version.

Three rules constrain it. The acceptance **due date is a calendar field** and **MUST NOT** be
treated as recorded assent — it never satisfies the acceptance instant owned by
[`05-preconditions`](./05-preconditions.md), and the cascade that fills it **MUST NOT** be read
as defaulting that instant. "When access begins" and "when billing begins" are **independent
axes**, so a service-activation date carries no billing implication of its own. And the quoted
service-activation date **MUST** be retained even when the activation barrier defers the line
past it, because the requested date is what the buyer agreed to and the actual instant is what
the subscription starts on.

**A missing required date is refused at the gate.** Where the policy switch requires a calendar
field and none can be resolved, submit **MUST** be refused with the registered
`date-cascade-invalid` reason, carried in the gate's all-failures report alongside every other
predicate failure ([`03-gate-and-pin`](./03-gate-and-pin.md) §3.6 *Run Gate and Submit*) rather
than returned ahead of the engine. The
rejected alternative was a twelfth order state holding such an order until the date arrives;
it was rejected because a data-entry omission would then acquire its own TTL, its own guards and
its own event, and because the PRD deliberately adds no waiting state for the analogous
mixed-date barrier. The order stays in `draft`, where it is already freely editable.

### 4.3 Field classification (normative)

Every authored field **MUST** be declared in exactly one class. The declaration is data read by
both this slice and [`04-versioning`](./04-versioning.md); neither maintains its own list, and
startup **MUST** fail if an authored field is unclassified.

**Commercial** — immutable from `submitted`; a change appends a new version and re-runs the
gate: line items and their membership, `qty`, `skuId`, `planId`, `priceId`, `payerTenantId`, the
three line dates, term duration, billing cycle, `category`, and the contract reference.

**Commercial-frozen** — commercial content that **MUST NOT** change at all after `submitted`,
even by amendment: `resourceTenantId` and `sellerTenantId`. PRD §6.1 fixes all three axes at
submit and permits exactly one post-submit mutation, `payerTenantId`, via amendment. A binary
commercial/administrative split had no way to express that, so both axes were classified merely
commercial and an amendment delta naming either was admissible — silently rebinding the resource
recipient or the selling party. The class exists so the classifier can refuse them
([`04-versioning`](./04-versioning.md) §4.1; [`../DECISIONS.md`](../DECISIONS.md) D-62).

**Read-through, never authored** — the contract-governed **auto-renewal election, term windows and
notice ladder**. PRD §6.1 says these "are read and displayed on the order, never authored by it".
The prohibition half holds structurally: no such field exists on the line. The display half is
owned by [`08-read-and-authz`](./08-read-and-authz.md) §4.2, which surfaces them from the
contracts port on a contracted order, so a buyer reading the order can see the renewal terms of
the deal they are committing to without this gear becoming a second authority for them.

**Administrative** — editable in any non-terminal state, audited, no version bump: external
references at order and line level, display labels, and internal notes. These live in the
**mutable** `orders_order_admin` and `orders_order_line_admin` tables, never on the append-only
version or line rows, which is what makes an in-place edit legal. The audit entry carries the
changed field with its prior and new value.

Two boundary cases are decided rather than left to reading. The **external reference is
administrative** even though it propagates to billing documents, because it is a buyer-side
reconciliation handle and correcting a mistyped purchase-order number should not invalidate an
approval. The **contract reference is commercial**, because the governing terms of a deal are
part of what was agreed.

### 4.4 Line shapes that are one line (normative)

A **bundle plan is one line item** and **MUST NOT** be expanded into component lines at capture.
The bundle is a first-class sellable plan with its own price basis and its own invoice
itemization owned by the pricing gear, and it spawns exactly one subscription.

**Non-subscription items** — setup fees, hardware, prepaid credit packs — are authored as
ordinary lines bound to a **one-time plan**, which the catalog supports and which Subscriptions
bills once at activation. There is no separate non-subscription line kind this phase; adding one
is an additive change once a Billing gear exists.

Both rules exist so that the 1:1 line-to-subscription cardinality holds without exception, which
every later slice relies on.

### 4.5 Draft abandonment

An abandoned `draft` is governed by the auto-void TTL owned by
[`07-hold-and-expiry`](./07-hold-and-expiry.md). This slice supplies the only input that sweep
needs — the creation instant on the aggregate — and states the outcome contract: an abandoned
draft is **auto-voided, never deleted**, so its audit trail survives. Nothing in this slice
schedules or performs the sweep.

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) — §6.1 order creation, line dates, term and cycle
- **Gear design**: [`../DESIGN.md`](../DESIGN.md) — realises `cpt-cf-bss-orders-lifecycle-component-capture`
- **Engine**: [`01-foundation`](./01-foundation.md) — transition contract, schema, reason registry
- **Consumers**: [`03-gate-and-pin`](./03-gate-and-pin.md) reads the policy-switch state and the authored lines; [`04-versioning`](./04-versioning.md) reads the field classification; [`07-hold-and-expiry`](./07-hold-and-expiry.md) reads the creation instant
- **Design set**: [`./README.md`](./README.md)
- **ADRs**: [`ADR/0001`](../ADR/0001-cpt-cf-bss-orders-lifecycle-adr-transition-through-engine.md) transition through the engine; [`ADR/0002`](../ADR/0002-cpt-cf-bss-orders-lifecycle-adr-slice-decomposition.md) the foundation-plus-seven-slices decomposition; [`ADR/0004`](../ADR/0004-cpt-cf-bss-orders-lifecycle-adr-closed-enumerations.md) the closed state and event enumerations