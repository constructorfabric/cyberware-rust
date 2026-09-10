<!-- CONFLUENCE_TITLE: [BSS]: Orders Lifecycle — Sellability Gate, Price Pin and Preview (Slice 3) -->
<!-- Related: ../DESIGN.md, ../PRD.md, ./README.md, ./01-foundation.md | Owners: BSS Orders team -->

# DESIGN — Sellability Gate, Price Pin and Preview (Slice 3)


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
  - [4.1 The adopted predicate set (normative)](#41-the-adopted-predicate-set-normative)
  - [4.2 The Orders delta (normative)](#42-the-orders-delta-normative)
  - [4.3 The catalog price pin (normative)](#43-the-catalog-price-pin-normative)
  - [4.4 The resolved total and TCV (normative)](#44-the-resolved-total-and-tcv-normative)
  - [4.5 What the order-time total excludes (normative)](#45-what-the-order-time-total-excludes-normative)
  - [4.6 Preview (normative)](#46-preview-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-design-gate-and-pin`

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice owns the single moment where an order stops being a draft and becomes a commitment.
It runs the sellability gate, captures the catalog price pin on every line, captures the
non-authoritative resolved total, and exposes the read-only Preview that answers "would this
pass, and what would it cost" without creating anything
([`../PRD.md`](../PRD.md) §6.1, §9.1).

Its central design choice is **adopt, don't fork**. The catalog predicates are not
re-implemented here: they are the published pricing sellability gate, invoked through a port,
and this slice adds only the delta the order boundary requires. Forking them would create a
second gate that drifts from the one Subscriptions enforces at `create`, which is precisely the
divergence the platform's seam discipline exists to prevent. The cost of adopting is inherited
honestly: pricing records that three of its six predicates are not yet evaluable from the built
read model, and an unevaluable predicate is a refusal — so this gate refuses in cases a fully
built catalog would admit, and that is correct behaviour rather than a defect.

The second choice is that **the pin and the state change are one commit**. The catalog price pin
is captured inside the submit transaction, so "submitted" and "pinned" are the same fact. A
submitted line without a resolvable pin is not a state the store can hold, which is how the
100 % pin-integrity guarantee becomes a database constraint rather than a policy.

The slice computes no price and performs no arithmetic over money. The resolved total **and** the
named total-contract-value figure both arrive from the price-evaluation contract and are stored as
received; the TCV's summation and its annualisation rule are the evaluation domain's obligation,
not this slice's ([`../DECISIONS.md`](../DECISIONS.md) D-40). Neither figure is a billing input.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|------------------|
| `cpt-cf-bss-orders-lifecycle-fr-order-submit` | The gate is a guard set on the single `draft → submitted` transition row. Every predicate is registered with the engine, so the transition cannot commit with a predicate unevaluated. |
| `cpt-cf-bss-orders-lifecycle-nfr-order-snapshot-integrity` | Pin capture is part of the submit contribution, and the pin column is NOT NULL for any version at `submitted` or beyond — the guarantee is a constraint, not a convention. |
| `cpt-cf-bss-orders-lifecycle-fr-orders-boundary-r4-no-price` | Prices are opaque references. The resolved total and the TCV figure both arrive computed from the evaluation contract and are stored as received; this slice derives nothing over money (D-40). |
| `cpt-cf-bss-orders-lifecycle-fr-order-tenant-axes` | Axis validity against IdP/Account Management is a gate predicate, and the axes freeze on the same commit that admits them. |
| `cpt-cf-bss-orders-lifecycle-fr-order-atomic-fulfillment` | The overlap and market predicates are re-evaluated immediately before the first activation intent, because both can change after the gate passes; each has its own fulfillment-time refusal reason. |
| `cpt-cf-bss-orders-lifecycle-interface-order-ops` | Preview runs the same predicate set and the same evaluation call with no transition, which is what makes it a truthful preview rather than a second implementation. |

#### NFR Allocation

| NFR ID | NFR Summary | Allocated To | Design Response | Verification Approach |
|--------|-------------|--------------|-----------------|----------------------|
| `cpt-cf-bss-orders-lifecycle-nfr-order-snapshot-integrity` | 100 % of submitted lines carry a resolvable pin | Pin capture | Captured in the submit transaction under a NOT NULL constraint for `submitted`+ versions; re-captured on every amendment | Invariant test asserting no `submitted`+ line exists without a pin; amendment test asserting re-pin |
| `cpt-cf-bss-orders-lifecycle-nfr-order-transition-latency` | Transition commit p95 < 1 s | Predicate orchestration | Every external input is resolved **before** the transaction opens, in parallel where independent; the transaction itself performs no network call | Load test measuring resolution and commit separately, so a slow catalog is attributable |
| `cpt-cf-bss-orders-lifecycle-nfr-order-read-latency` | Read p95 < 200 ms | Preview | Preview is bounded by the evaluation contract's own latency and is explicitly excluded from the order-read budget, since it is a computation and not a read | Benchmark reported against the evaluation contract's budget, not the read budget |

#### Key ADRs

The seven gear ADRs govern this slice. Two decisions taken here are recorded in the register rather than as ADRs: **all-failures
reporting** rather than short-circuit evaluation ([`../DECISIONS.md`](../DECISIONS.md) D-75,
§4.2), and **the order-time total excludes subscription-scoped overlays** and says which (D-76,
§4.5). The fail-closed posture on an unevaluable gate input carries its own
[`ADR/0003`](../ADR/0003-cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate.md).

### 1.3 Architecture Layers

Inherited from [`01-foundation`](./01-foundation.md) §1.3. This slice adds **six** outbound ports at the
infrastructure layer — the catalog predicate port, the evaluation port, the identity port, the
overlap-presence port, the contract-resolution port and the indicative-tax port (Preview only) —
all invoked before the transition transaction opens.

## 2. Principles and Constraints

### 2.1 Design Principles

#### Adopt the catalog gate, never fork it

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-adopt-not-fork-gate`

The catalog predicates are invoked through a port against the published pricing gate. This slice
holds no copy of them, no partial re-implementation and no local override. Where the adopted
gate is stricter than an order-side reading would be, the adopted gate wins. A local fork would
be undetectable at review time and would surface as a purchase that passed the order gate and
failed at subscription `create` — the exact failure mode the two-phase fulfillment design spends
its complexity avoiding.

#### The pin is the commit

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-pin-is-the-commit`

Pin capture and the state change share one transaction. There is no window in which an order is
`submitted` but unpinned, and no repair path that pins retroactively. An amendment re-pins as
part of its own commit, so every version's pin is contemporaneous with that version.

#### Resolve outside, decide inside

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-resolve-outside-decide-inside`

Every external input — catalog predicates, axis validity, contract status, evaluation output,
overlap presence — is resolved before the transaction opens and enters the guard as a plain
value. Nothing in the commit path makes a network call. This is what keeps the p95 commit budget
achievable with a slow upstream, and it is why an unreachable dependency degrades this
capability rather than stalling the gear.

#### Preview and submit share one implementation

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-preview-shares-implementation`

Preview calls the same predicate set and the same evaluation contract as submit, differing only
in that it creates no order or commercial artifact; it persists its bounded-retention gate outcome
rows for observability. A second implementation would drift, and a
preview that disagrees with submit is worse than no preview.

### 2.2 Constraints

#### Ports are bounded by deadline, breaker and bulkhead

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-port-budgets`

A **slow** port is the common failure, not an unavailable one, and resolving inputs outside the
transaction bounds the *commit* path without bounding caller-visible latency. Every port therefore
carries a **deadline**. Five of the six sit on the submit path; the indicative-tax port is
**Preview only**, so the two surfaces carry two budgets:

| Port | Deadline |
|------|----------|
| Catalog predicates | 250 ms |
| Identity and party eligibility | 250 ms |
| Price evaluation | 500 ms |
| Overlap presence | 250 ms |
| Contract resolution | 250 ms |
| Indicative tax (Preview only) | 250 ms |
| **Submit budget** (five ports; tax not invoked) | **1.5 s** |
| **Preview budget** (all six) | **1.75 s** |

This budget covers **port resolution only**, which completes before the transaction opens. The
PRD's `p95 < 1 s` is scoped to the commit — the durable write and event publish — so the two are
compliant side by side, but nothing bounds what the caller experiences end to end. That gap is
routed as [`../DECISIONS.md`](../DECISIONS.md) Q-11 rather than resolved here, and this design
makes no claim that a submit returns within one second.

A deadline elapsing is a **refusal** carrying that port's unavailability reason, identical to
unreachability, because what matters is evaluability rather than the reason for silence. Retry is
bounded at two attempts on transient failure only and never on a deadline. Each port carries a
**circuit breaker** opening at a **rolling failure ratio of 0.5 over a 30-second window**, held
open for 10 seconds and mapping to the same reason, and a **concurrency bulkhead of 32 in-flight
calls per port**, so one slow upstream cannot exhaust the request pool for the others. Submit
carries a **rate limit of 10 per minute per caller** and Preview **60 per minute per caller** —
Preview is the cheapest endpoint to call and the most expensive to serve, so it is limited
separately rather than inheriting the submit figure. These are working baselines set here for the
same reason the page-size bounds are: a threshold nobody set is a threshold nobody can verify
against, and ratification sits with Architecture
([`../DECISIONS.md`](../DECISIONS.md) Q-26). The platform API
baseline requires.

#### Three adopted predicates are not evaluable today

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-partial-predicate-evaluability`

The pricing decision register records that the six-predicate promise is not yet met: three
predicates cannot be evaluated from the built read model. Pricing's own posture is that an
unbuilt predicate lane is indistinguishable from one that timed out, because what matters is
evaluability rather than the reason for silence. This gate inherits that: such a predicate is a
**refusal**, carrying the adopted gate's reason unchanged. Operators will see submits refused
for predicates that are not yet implemented upstream, and that is the designed behaviour.

#### The overlap check depends on an unagreed upstream read

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-overlap-read-unagreed`

The against-existing-subscriptions half of the overlap rule requires a presence read on the
Subscriptions gear — registered upstream as `SUB-O5`, unagreed, and against a gear with no
implementation. Until it exists the within-basket half is fully evaluable and the
against-existing half is **unevaluable and therefore a refusal**, which fails closed and is
consistent with §2.1. The port is defined so that agreement upstream is a boundary change. The
fallback of admitting the submit and relying on the activation-time re-check is **rejected**: it
would move the failure past the first line's provisioning, into precisely the expensive
compensation path the design exists to avoid.

#### The default overlap key collides in the partner path

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-overlap-key-partner-collision`

The default `overlapScopeKey` is `(payerTenantId, catalogSubscriptionProductKey)` at cardinality
one, adopted from the subscriptions gear. Read literally it refuses a partner admin buying the
same product for a second customer tenant, because the payer is the same — which contradicts the
partner path the PRD's own actor model assumes. This slice **MUST NOT** fork the default locally;
the dimension binding is an upstream question. Until it is answered the key is applied as
adopted and the collision is a known, reported refusal rather than a silent local widening.

#### The order-time total is incomplete by construction

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-total-excludes-subscription-overlays`

Overlays scoped to a subscription — brand being the named case — need evaluation context that
does not exist before a subscription does. No pre-subscription evaluation operation exists, so
the order-time total **excludes** them and §4.5 names the exclusion explicitly. The pin is
unaffected: it freezes only the catalog-written segment, which is fully resolvable at submit.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-catalog-price-pin`

The catalog-written segment of the downstream composed pricing snapshot, frozen per line at
submit: the committed catalog version, the resolved price identifiers including cohort, and the
evaluation-policy version. It is the whole of what an order captures about price, and it is
never the composed snapshot reference.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-order-market`

The derived, non-authoritative `(currency, region)` binding computed at submit from the
**payer's** commercial profile. Gate currency and region checks are consistency assertions
against it. The authoritative binding is frozen downstream by Subscriptions at activation, so
this entity is evidence of what was assumed, not a claim about what will hold.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-gate-outcome`

The per-line and per-order result of one gate run: each predicate's identity, its verdict, and
its reason where it failed. Retained for a refused submit as well as an admitted one, so a
partner can be told everything that is wrong in one response.

It also populates the line pins and the resolved total whose schema is specified in
[`01-foundation`](./01-foundation.md) §3.7.

**Relationships**:
- `Order line` → `Catalog price pin`: one-to-one per version; NOT NULL from `submitted` onward.
- `Order version` → `Order market`: one-to-one, recomputed on each amendment.
- `Gate outcome` → `Order version`: one-to-one for an admitted submit; standalone for a refusal and for every Preview.

### 3.2 Component Model

This slice realises `cpt-cf-bss-orders-lifecycle-component-gate-and-pin`
([`../DESIGN.md`](../DESIGN.md) §3.2) as three internal parts.

#### Predicate orchestrator

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-gate-predicate-orchestrator`

##### Why this component exists

The gate has two sources of truth — the adopted catalog predicates and the Orders delta — and
one answer to produce. Something has to resolve both, in parallel where independent, and combine
them into a single verdict without letting either source's latency dominate.

##### Responsibility scope

Resolution of every predicate input before the transaction opens; parallel dispatch of the
independent ports; the adopted-predicate invocation; evaluation of the nine delta predicates;
the all-failures collection contract; and the combined verdict.

##### Responsibility boundaries

It authors no adopted predicate and copies none. It captures no pin and computes no total. It
does not decide whether approval is required — Preview and submit both return no approval
verdict.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-orchestrator` — depends on
- `cpt-cf-bss-orders-lifecycle-component-gate-pin-capture` — calls
- `cpt-cf-bss-orders-lifecycle-component-gate-preview` — shares model with

#### Pin and total capture

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-gate-pin-capture`

##### Why this component exists

Price integrity between capture and activation is the revenue-integrity risk the gear exists to
close, and it can only be closed atomically with the state change.

##### Responsibility scope

Pin composition from the resolved catalog data and the resolvability check; and persistence of
the resolved total as received — gross, net, the discount component, the promotion reference, the
four charge-kind rows and the TCV figure. The summation and annualisation behind that figure are
performed by the evaluation contract, not here (D-40).

##### Responsibility boundaries

It computes no price and applies no overlay. Usage carries no committed amount and is excluded
from TCV. Nothing it stores may be read as a billing input.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-gate-predicate-orchestrator` — depends on

#### Preview

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-component-gate-preview`

##### Why this component exists

A buyer needs to know whether a basket is purchasable and what it costs before committing to it,
and the answer must be the same answer submit would give.

##### Responsibility scope

The read-only run over a supplied basket; per-line gate results; the resolved total including
TCV; the indicative tax figure obtained from the tax owner and never stored; expected
fulfillment time and per-line deferral where line dates differ.

##### Responsibility boundaries

It creates and mutates nothing, stores no tax, and returns no approval-requirement verdict. It
refuses to return a TCV figure when a basket line omits term duration or billing cycle.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-gate-predicate-orchestrator` — depends on

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-interface-gate-ops`

- **Requirement**: `cpt-cf-bss-orders-lifecycle-interface-order-ops`
- **Technology**: REST/OpenAPI via `OperationBuilder`; RFC 9457 problems

| Method | Path | Description | Stability |
|--------|------|-------------|-----------|
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/submit` | Run the gate, capture pin and total, transition `draft → submitted` | unstable |
| `POST` | `/bss-orders-lifecycle/v1/orders/preview` | Run the gate and evaluation over a supplied basket, creating no order; rate-limited, and open to partner admin, direct customer and seller operator | unstable |

**Reasons contributed to the registry**: axis-invalid, contract-not-active, quantity-below-floor,
market-inconsistent, reference-unresolvable, reference-duplicated, currency-mixed,
overlap-cardinality-exceeded, order-in-flight-for-key, pin-unresolvable, no-lines,
preview-term-or-cycle-missing; and exactly one unavailable reason for each port:
catalog-predicates-unavailable, identity-party-unavailable, contract-resolution-unavailable,
overlap-presence-unevaluable (to which the overlap predicate's unevaluable outcome also resolves,
so the condition has one name and not two), evaluation-unavailable and indicative-tax-unavailable. Adopted
catalog predicate failure reasons are passed through unchanged.

**Fulfillment-time reasons**, raised by [`06-workflow-seam`](./06-workflow-seam.md) using
predicates owned here: market-divergence, overlap-collision. The date predicate contributes
`required-line-date-unresolved`.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-interface-gate-ports`

**Six** outbound ports, all invoked before the transaction opens and all bounded by §2.2: the
**catalog predicate port** (the adopted gate), the **evaluation port** (resolved total and the TCV
figure), the **identity port** (axis validity, party eligibility and the payer's commercial
profile), the **overlap-presence port** (`SUB-O5`, unagreed), the **contract-resolution port** (contract
active, and party eligibility where a contract is referenced), and the **tax port** (the
indicative figure Preview returns and never stores). Each port's unavailability or deadline maps to its own
reason, so an operator can tell which upstream refused.

### 3.4 Internal Dependencies

| Dependency Gear | Interface Used | Purpose |
|-------------------|----------------|----------|
| `toolkit-db` | Runtime-scoped access, via the engine | Pin, total and gate-outcome persistence inside the transition transaction |

### 3.5 External Dependencies

#### Pricing and catalog

| Dependency Gear | Interface Used | Purpose |
|-------------------|---------------|---------|
| `pricing` | SDK client | The adopted sellability predicates, and the catalog data the pin is composed from |
| `rating` | SDK client | The price-evaluation contract producing the resolved total; composition owner of the full snapshot, which this slice never stores |

#### Identity, contracts and fulfillment

| Dependency Gear | Interface Used | Purpose |
|-------------------|---------------|---------|
| `account-management` | SDK client | Tenant-axis validity and the payer's commercial profile behind the order market |
| `contracts` | SDK client | Contract status where a reference is present; party-eligibility policy is unimplemented there, and its unevaluability is a refusal |
| `subscriptions` | SDK client | The overlap-presence read (`SUB-O5`), unagreed and unimplemented |
| Billing-chain tax owner | SDK client | The indicative tax figure Preview returns and never stores |

**Dependency Rules** (per project conventions):
- No circular dependencies
- Always use SDK modules for inter-gear communication
- No cross-category sideways deps except through contracts
- Only integration/adapter gears talk to external systems
- `SecurityContext` must be propagated across all in-process calls

### 3.6 Interactions and Sequences

#### Submit through the gate

**ID**: `cpt-cf-bss-orders-lifecycle-seq-gate-submit`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-new-acquisition`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-partner-admin`, `cpt-cf-bss-orders-lifecycle-actor-orders-catalog-pricing`, `cpt-cf-bss-orders-lifecycle-actor-orders-idp-ams`, `cpt-cf-bss-orders-lifecycle-actor-orders-contracts`

**Algorithm: Run Gate and Submit**

Input: order_id, security_context, idempotency_key, expected_version
Output: submitted order with pin and total, or a refusal listing every failure

1. [ ] - `p1` - Declare the at-least-one-line guard so the engine evaluates it and audits its refusal - `inst-gs-declare-lines-guard`
2. [ ] - `p1` - Derive the order market from the payer's commercial profile - `inst-gs-derive-market`
3. [ ] - `p1` - **PARALLEL** resolve the independent port inputs: - `inst-gs-parallel-resolve`
   - [ ] - `p1` - adopted catalog predicates over every line's scope key - `inst-gs-resolve-catalog`
   - [ ] - `p1` - tenant-axis validity and party eligibility - `inst-gs-resolve-identity`
   - [ ] - `p1` - contract status, only where a contract reference is present - `inst-gs-resolve-contract`
   - [ ] - `p1` - overlap presence for each line's overlap key - `inst-gs-resolve-overlap`
   - [ ] - `p1` - the resolved total from the evaluation contract - `inst-gs-resolve-total`
4. [ ] - `p1` - Read each line's tenant policy switch and resolve the date cascade per [`02-capture`](./02-capture.md) §4.2, using the submit instant where the contract-effective date is unauthored - `inst-gs-resolve-cascade`
5. [ ] - `p1` - Initialize an empty failure list - `inst-gs-init-failures`
6. [ ] - `p1` - **FOR EACH** adopted predicate result that failed: add it with its reason unchanged - `inst-gs-collect-adopted`
7. [ ] - `p1` - **FOR EACH** of the nine delta predicates: evaluate and add any failure - `inst-gs-collect-delta`
8. [ ] - `p1` - **IF** any port was unresolvable: add its unevaluable reason (absence is a refusal) - `inst-gs-collect-unevaluable`
9. [ ] - `p1` - **FOR EACH** line whose policy-required calendar field cannot be resolved: add date-cascade-invalid - `inst-gs-collect-cascade-invalid`
10. [ ] - `p1` - **IF** the failure list is non-empty: - `inst-gs-if-failures`
    1. [ ] - `p1` - Pass the failure set as the contribution so the engine persists the gate outcome, audits the refusal and settles the idempotency record in one transaction - `inst-gs-contribute-refusal-outcome`
    2. [ ] - `p1` - **RETURN** every failure in one response; the order stays in `draft` - `inst-gs-return-all-failures`
11. [ ] - `p1` - Compose the catalog price pin for each line and verify it is resolvable - `inst-gs-compose-pin`
12. [ ] - `p1` - **IF** any pin is unresolvable: - `inst-gs-if-pin-unresolvable`
    1. [ ] - `p1` - Pass pin-unresolvable as the contribution so the engine audits the refusal and settles the idempotency record, exactly as step 10.1 does - `inst-gs-contribute-pin-refusal`
    2. [ ] - `p1` - **RETURN** the pin-unresolvable refusal; the order stays in `draft` - `inst-gs-return-pin-unresolvable`
13. [ ] - `p1` - Assemble the resolved total rows and the TCV figure per §4.4 - `inst-gs-assemble-total`
14. [ ] - `p1` - Request the submit transition, contributing pin, total, market, the resolved line dates and policy-switch state, gate outcome and the **submitting principal as the initiating actor** - `inst-gs-request-transition`
15. [ ] - `p1` - **RETURN** the submitted order - `inst-gs-return-submitted`

**Description**: Steps 5 through 10 are the all-failures contract. Every input is resolved before
*Run Gate and Submit* step 14, so the transaction that commits `submitted` performs no network call and the pin lands
in the same commit as the state.

#### Preview a basket

**ID**: `cpt-cf-bss-orders-lifecycle-seq-gate-preview`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-new-acquisition`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-partner-admin`, `cpt-cf-bss-orders-lifecycle-actor-orders-direct-customer`

```mermaid
sequenceDiagram
    participant B as Buyer surface
    participant P as Preview
    participant C as pricing / rating
    participant T as tax owner
    B ->> P: basket lines (term + cycle required for TCV)
    P ->> C: adopted predicates + evaluation
    C -->> P: per-line verdicts, resolved total
    P ->> T: indicative tax
    T -->> P: indicative amount
    P -->> B: verdicts, total, TCV, indicative tax, expected fulfillment time, per-line deferral
```

**Description**: No order is created and no order is mutated; the indicative tax figure is never stored. The gate outcomes are persisted under their own bounded retention. Preview returns no
approval-requirement verdict — that belongs to the policy owner and is obtained by the sibling
gear — and it withholds TCV entirely when a line omits term or cycle, rather than reporting a
figure computed from an assumed term.

#### Re-check before first activation

**ID**: `cpt-cf-bss-orders-lifecycle-seq-gate-fulfillment-recheck`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-fulfillment-complete`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-workflow`, `cpt-cf-bss-orders-lifecycle-actor-orders-subscriptions`

**Algorithm: Re-check Activation Preconditions**

Input: order_id, current_version
Output: proceed, or a per-line rejection reason

1. [ ] - `p1` - Re-derive the order market from the payer's **current** commercial profile - `inst-rc-rederive-market`
2. [ ] - `p1` - **IF** it diverges from the market frozen at submit: - `inst-rc-if-market-diverged`
   1. [ ] - `p1` - **RETURN** market-divergence rejection for the affected lines - `inst-rc-return-market-divergence`
3. [ ] - `p1` - Re-read overlap presence for each line's overlap key - `inst-rc-reread-overlap`
4. [ ] - `p1` - **IF** a collision appeared since the gate: - `inst-rc-if-overlap-collision`
   1. [ ] - `p1` - **RETURN** overlap-collision rejection for the affected lines - `inst-rc-return-overlap-collision`
5. [ ] - `p1` - **RETURN** proceed - `inst-rc-return-proceed`

**Description**: Both predicates read state owned elsewhere that can move between submit and
activation, which is why passing the gate is necessary but not sufficient. The sibling gear
invokes this immediately before dispatching the first activation intent and treats either
rejection as a pre-activation abort rather than a line-execution failure.

### 3.7 Database Schemas and Tables

This slice introduces one table and owns columns on two specified in
[`01-foundation`](./01-foundation.md) §3.7: `orders_order_line.catalog_price_pin` and the whole
of `orders_resolved_total`.

#### Table: orders_gate_outcome

**ID**: `cpt-cf-bss-orders-lifecycle-dbtable-gate-outcome`

**Schema**:

| Column | Type | Description |
|--------|------|-------------|
| outcome_id | uuid | Outcome identity |
| order_id | uuid, nullable | NULL for a Preview run, which has no order |
| version | integer, nullable | The version admitted, where the run admitted one |
| line_id | uuid, nullable | NULL for order-level predicates |
| predicate | text | Predicate identity, adopted or delta |
| verdict | enum | `passed`, `failed` or `unevaluable` |
| reason | text, nullable | Registered reason on failure or unevaluability |
| evaluated_at | timestamptz | Run instant |

**PK**: outcome_id

**Constraints**: append-only; `reason` NOT NULL when `verdict` is not `passed`; indexed on
`(evaluated_at) WHERE order_id IS NULL` for the 7-day Preview purge, and on `(order_id)` for the
per-order diagnostic read this table exists to serve. Row volume is predicates x lines per
attempt, on refusals as well as admissions, so it is the highest-growth table in the gear and the
one least able to afford a sequential scan per purge run.

**Additional info**: retained for refused submits as well as admitted ones, which is what makes
"why was this refused last Tuesday" answerable. Preview outcomes carry a shorter retention than
order-linked ones, since they reference no commercial artifact.

The **order market** is stored per version as `orders_order_version.market_currency` and
`orders_order_version.market_region`, recomputed and re-stored on each amendment so a later market
does not overwrite the one a prior version was gated against.

### 3.8 Deployment Topology

Inherited from [`01-foundation`](./01-foundation.md) §3.8. This slice adds no background worker;
its six ports are called synchronously on the submit and Preview paths under the budgets of §2.2.

**Observability owned here**: per-port latency, timeout and breaker-state series, so a slow
upstream is attributable rather than surfacing as an unexplained submit latency; gate-refusal
counts broken down by predicate and by adopted-versus-delta origin; the **unevaluable-predicate
rate**, which is the signal that an upstream lane is missing rather than failing; the
pin-unresolvable rate; Preview call rate against its limit and its 1.75 s budget; and the
submit-path total against the 1.5 s submit budget. Alerts fire on any breaker opening, on the unevaluable-predicate rate crossing its
threshold, and on pin-unresolvable being non-zero — the last because it means the price-integrity
guarantee is refusing real purchases.

## 4. Additional Context

### 4.1 The adopted predicate set (normative)

The catalog half of the gate is **adopted by reference** and comprises the published pricing
predicates: committed catalog version, lifecycle-not-retired, per-market general availability,
registry sellability, active windows, and the full conjunction over every scope key the purchase
binds. This slice **MUST NOT** hold a copy, a subset or a local override of any of them, and
**MUST** pass their reasons through unchanged rather than re-labelling them.

Where an adopted predicate is **unevaluable** — because its lane is not built upstream, or
because the port is unreachable — the outcome is a refusal carrying that predicate's reason. The
gate **MUST NOT** treat unevaluable as passed under any configuration.

### 4.2 The Orders delta (normative)

Nine predicates are this gear's own. Each **MUST** carry its own machine-readable reason:

1. **Axis validity** — all three tenant axes resolve against IdP/Account Management.
2. **Contract active** — where a contract reference is present it resolves to an **active** contract. Party-eligibility policy is owned by Contracts and consulted only when a contract is referenced; its unevaluability there is a refusal. This is a predicate of its own because it carries its own registered reason, and folding it into axis validity was what made the predicate count and the reason count disagree.
3. **Purchase-quantity floor** — `qty` satisfies the floor the price row declares, and the declared bounds on one-time plans. There is **no plan-level maximum**: upper bounds are resource quotas enforced at fulfillment, not here.
4. **Order-market consistency** — each line's currency and region are consistent with the market derived from the **payer's** profile, which in the partner path is not the calling tenant's.
5. **Reference resolution** — every `skuId`, `planId` and `priceId` resolves, with no collision or duplication across lines.
6. **Single currency** — all lines share one currency. Authoring already refuses a mixed basket; this predicate is the backstop for a basket assembled before the rule existed.
7. **Overlap uniqueness** — projected activation does not violate the configured concurrent-active cardinality per `overlapScopeKey`, evaluated **within the basket** and **against existing subscriptions**.
8. **Required line dates resolved** — where the stored per-line date-policy-switch state requires a service-activation or customer-acceptance date, that date resolves to a calendar value. A line missing a policy-required date is refused here rather than held in a waiting state ([`../DECISIONS.md`](../DECISIONS.md) D-60; the rejected alternative was a twelfth order state).
9. **One in-flight order per overlap key** — **no other order** in the in-flight set holds the same key. The exclusion of the requesting order itself is load-bearing: an amendment is issued by an order that is *already* in-flight and already holds its key, so a predicate counting all holders without excluding the subject refuses every amendment against itself. Predicate 7 bounds concurrent **subscriptions** and is configurable via `maxConcurrentActive`; this one bounds concurrent **orders** and PRD §6.1(g) fixes it at one with no configurability clause. The two are deliberately separate rules and **MUST NOT** be given a shared cardinality (D-83). **The in-flight set is `submitted`, `pending_approval`, `approved`, `in_fulfillment` and `on_hold`** — `on_hold` is included because a held order resumes onto its pre-hold state and still holds its key, so excluding it would admit a second order that collides at the activation re-check, the expensive path §2.2 refuses to defer failures into. The partial unique index in [`01-foundation`](./01-foundation.md) §3.7 covers exactly those five states. Idempotency keys protect against a repeated call; this protects against **more distinct orders on one key than the key permits**. The resolved key is **persisted** on the line and the rule is enforced by an `orders_inflight_overlap_claim`
**partial unique index over `(payer_tenant_id, overlap_scope_key)` inside the transition
transaction**; this predicate is the friendly pre-check,
not the enforcement, because a predicate resolved outside the transaction would let two concurrent
identical submits both pass. The transaction creates claims on submit or amendment and releases
them only on a terminal transition ([`../DECISIONS.md`](../DECISIONS.md) D-26).

**All failures are reported together.** The gate **MUST** evaluate every predicate it can and
return every failure in one response, rather than short-circuiting on the first. The rejected
alternative was short-circuit evaluation; it was rejected because a five-line basket with three
independent problems would otherwise require three round trips to discover, and because the
predicates are resolved in parallel anyway, so the marginal cost of completing evaluation is
near zero. A single failure still refuses the whole order — reporting is exhaustive, admission
is not partial.

### 4.3 The catalog price pin (normative)

The pin **MUST** contain exactly the catalog-written segment: the **committed** catalog version,
the resolved price identifiers including cohort, and the evaluation-policy version. It **MUST
NOT** contain the composed pricing snapshot reference, an overlay outcome, a coupon, an
FX lock or a commitment — those segments are written downstream by Subscriptions at activation
and by Rating at evaluation, and Rating is their composition owner.

A pin is **resolvable** when every identifier in it addresses a committed catalog row and the
catalog version is committed rather than pending. Capture **MUST** occur inside the submit
transaction; a line whose pin is not resolvable **MUST** refuse the submit. An amendment **MUST**
re-pin as part of its own commit.

Known staleness is accepted, and **what bounds it is the per-state TTL**: an order cannot sit in
`submitted` or `approved` past its TTL, so that is the outer limit on how stale a pin can be when
fulfilment begins — which also means the bound is only as real as the TTL, currently an unset
Product-owned value (Q-06). PRD §16 additionally asks for an acceptable staleness window to be
documented in the NFR workshop; that is routed as Q-17 rather than dropped. If the catalog
publishes a change after submit, the
pinned rows are stale relative to the newest version and the customer binds to the pinned rows.
The mitigations are the mandatory re-pin on amendment and the downstream seal at activation.

### 4.4 The resolved total and TCV (normative)

The total is **non-authoritative** and is stored as received from the evaluation contract. Per
line and per order it **MUST** carry gross and net figures, an explicit discount component with
its promotion reference where one applied, and all four charge kinds named separately:
`recurring`, `usage`, `one_time` and `one_time_setup`. **Tax MUST NOT** be included — the total
is explicitly pre-tax, and tax is computed by the billing chain at invoice time. **Usage carries
no committed amount**, is excluded from the total and **MUST** be flagged as excluded.

The single named figure exposed to approval policy is **net pre-tax total contract value**, which
**arrives computed from the price-evaluation contract and is stored verbatim** — this gear performs
no arithmetic over money, so the formula below is reproduced from the PRD glossary for the reader
rather than as an instruction to this slice ([`../DECISIONS.md`](../DECISIONS.md) D-40). Per line
it is `recurring × periods-in-term + one_time + one_time_setup`, summed across lines, with usage
excluded. For an **open-ended or rolling term** with no finite periods-in-term, the
recurring component **MUST** be annualised at that line's cycle — 12 monthly, 4 quarterly, 1
annual — so the figure is defined and two rolling deals differing only in cycle stay comparable;
`one_time` and `one_time_setup` are still added once. The per-period recurring amount **MUST**
remain stored in the charge-kind decomposition for display.

**TCV is not deal value.** A predominantly usage-based order presents a low or zero TCV, because
committed usage is not representable on the line this phase. The figure **MUST NOT** be read as
the commercial size of the deal, and it **MUST NOT** be used as a billing input. Which threshold
it is compared against is owned by the approval policy owner and is not defined here.

### 4.5 What the order-time total excludes (normative)

Overlays requiring subscription-level evaluation context **MUST** be excluded from the order-time
total, and the exclusion **MUST** be stated on the read and Preview responses rather than left
implicit. The named case is **brand**, whose per-sale identifier is owned by Subscriptions and
does not exist before a subscription does.

This resolves **PRD §15 row 6** in the only way available without a new upstream
operation: rather than reporting a total that silently omits an overlay, the response declares
what it omitted. Should a pre-subscription evaluation operation later accept order-level scope
inputs, this exclusion becomes removable without changing the pin, which is unaffected because
it freezes only the catalog-written segment.

### 4.6 Preview (normative)

Preview **MUST** create no order and mutate no order. It **does** persist its per-predicate gate
outcomes to `orders_gate_outcome` with a **7-day retention** and a rate limit, because "why was
this basket refused last Tuesday" is a support question worth answering and an unbounded
unauthenticated write path is not ([`../DECISIONS.md`](../DECISIONS.md) D-52). It **MUST** return
per-line gate results, the
resolved total including the named TCV figure, an **indicative** tax amount per line and in
total sourced from the tax owner, and — when line service-activation dates differ — expected
fulfillment time plus a per-line deferral where a quoted date is earlier.

**Preview is subject to the delegation-proof rule.** A Preview naming any tenant axis outside the
caller's own **MUST** carry delegation proof and **MUST** refuse without it, because the gate it
runs resolves party eligibility, contract status, the payer-derived market and overlap presence —
each of which discloses a fact about the named tenant
([`08-read-and-authz`](./08-read-and-authz.md) §2.2).

Three prohibitions are absolute. The indicative tax **MUST NOT** be stored on any order. Preview
**MUST NOT** return an approval-requirement verdict. And Preview **MUST NOT** return a TCV figure
when a basket line omits term duration or billing cycle — the figure is undefined without them,
and returning one computed from an assumed term would be a worse answer than none.

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) — §6.1 submit gate and pin, §9.1 Preview
- **Gear design**: [`../DESIGN.md`](../DESIGN.md) — realises `cpt-cf-bss-orders-lifecycle-component-gate-and-pin`
- **Engine**: [`01-foundation`](./01-foundation.md) — transition contract, reason registry, resolved-total schema
- **Producers**: [`02-capture`](./02-capture.md) authors the lines and the policy-switch state this gate reads
- **Consumers**: [`04-versioning`](./04-versioning.md) re-runs this gate on amendment; [`05-preconditions`](./05-preconditions.md) contributes the self-service acceptance instant to the submit transition this slice owns; [`06-workflow-seam`](./06-workflow-seam.md) invokes the activation re-check
- **Upstream asks**: `SUB-O5` overlap presence; the overlap **dimension** binding for the partner path
- **ADRs**: [`ADR/0001`](../ADR/0001-cpt-cf-bss-orders-lifecycle-adr-transition-through-engine.md) transition through the engine; [`ADR/0002`](../ADR/0002-cpt-cf-bss-orders-lifecycle-adr-slice-decomposition.md) the foundation-plus-seven-slices decomposition; [`ADR/0003`](../ADR/0003-cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate.md) fail closed on an unevaluable gate input; [`ADR/0007`](../ADR/0007-cpt-cf-bss-orders-lifecycle-adr-in-transaction-concurrency.md) concurrency enforced by the in-transaction overlap constraint behind predicate 9