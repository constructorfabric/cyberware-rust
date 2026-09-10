Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Subscriptions — Event Model & Billing Alignment (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Contracts (PriceOverride) | Downstream: Rating, Billing, OSS, Policy Engine, Analytics | Owners: BSS Subscriptions team -->

# DESIGN — Event Model & Billing Alignment (Slice 8)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-design-events-billing`

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
  - [4.1 Producer Inventory and Payload Sufficiency (normative)](#41-producer-inventory-and-payload-sufficiency-normative)
  - [4.2 Ordering (normative)](#42-ordering-normative)
  - [4.3 Recurring Idempotency and No Retro-Edit (normative)](#43-recurring-idempotency-and-no-retro-edit-normative)
  - [4.4 Traceability (normative)](#44-traceability-normative)
  - [4.5 Dataset Separation and PriceOverride Consumption (normative)](#45-dataset-separation-and-priceoverride-consumption-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice is the **outbound integration substrate**: the lifecycle **event producer inventory**, the
ordering guarantee, the recurring `BillableItem` idempotency + no-retro-edit invariants, and the
charge-to-catalog traceability tuple. The PRD fixes **sufficiency, not schema** — every lifecycle
event carries enough identity/tenancy/correlation/time context to route, deduplicate, and replay;
composition-changing events additionally carry enough snapshot-oriented commercial context for rating
and Billing to stay aligned and idempotent ([`../PRD.md`](../PRD.md) §6.7, §6.8). **This slice owns
the event naming registry and the field matrix** (§4.1, SUB-D-09);
[`09-consumer-contracts.md`](./09-consumer-contracts.md) owns only the wire mappings.

Two seams meet here: **SUB-B1** (recurring idempotency `(subscriptionId, billing period, lineKey)` — per component, SUB-D-19,
posted-invoice immutability, `{subscriptionId, skuId, planId, priceId}` + `pricingSnapshotRef`
traceability) and the ordering half of **SUB-R1** (the pinned `(orderingTenantId, subscriptionId)` key shared
with rating); it also consumes **SUB-C5** (Contract `PriceOverride` windows).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-subscriptions-fr-event-producers` | The frozen producer set (`SubscriptionCreated`/`Activated`/`Suspended`/`Resumed`/`Cancelled`/`PlanChanged`, `BillableItemCreated(recurring)`, `EntitlementIssued`/`Revoked`, `OwnershipTransfer*`) emitted via the outbox (§4.1). |
| `cpt-cf-bss-subscriptions-fr-event-payload-completeness` | Sufficiency rule (not schema): identity/tenancy/correlation/time for route/dedup/replay; snapshot-oriented commercial context on composition-changing events (§4.1). |
| `cpt-cf-bss-subscriptions-fr-event-ordering` | Ordered within the pinned `(orderingTenantId, subscriptionId)` (SUB-D-06) — the key shared with rating partition ordering (§4.2). |
| `cpt-cf-bss-subscriptions-fr-recurring-idempotency` / `cpt-cf-bss-subscriptions-fr-no-retro-edit` | `BillableItem(kind=recurring)` idempotent per `(subscriptionId, billing period, lineKey)` (per billable component — SUB-D-19); posted lines never rewritten — corrections are new billable/adjustment artifacts (§4.3). |
| `cpt-cf-bss-subscriptions-fr-billing-traceability` / `cpt-cf-bss-subscriptions-fr-dataset-separation` | Every item traces to `{subscriptionId, skuId, planId, priceId}` + `pricingSnapshotRef`; subscription state ≠ posted invoice state (§4.4, §4.5). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-subscriptions-nfr-operational-baselines` | Event outbox | Event delivery to consumers p95 < 30s; at-least-once + dedupable | Load test; baseline (workshop-pending) |
| `cpt-cf-bss-subscriptions-nfr-recurring-cut` | Recurring emission | Daily generation cut; zero duplicates via the idempotency key | Reconciliation §17.1 |

#### Key ADRs

No slice-local ADR; the ordering key + recurring idempotency are manifest invariants shared with
rating/Billing (SEAMS **SUB-R1**, **SUB-B1**).

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-tech-stack-evt`

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Producer emission + payload assembly; recurring idempotency keying; traceability stamping | Rust module in the `subscriptions` gear |
| Domain | Event envelopes, `BillableItem` recurring key, traceability tuple | Rust; GTS + Rust domain structs |
| Infrastructure | Transactional event outbox (committed with the transition) | PostgreSQL, SecureORM |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Sufficiency, not schema

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-sufficiency-evt`

The PRD contract is payload **sufficiency** (route/dedup/replay; snapshot-oriented context on
composition changes); the **field matrix is owned here** (§4.1, with the naming registry —
SUB-D-09), while the **wire format** (CloudEvents extensions, header bindings) is
[`09-consumer-contracts.md`](./09-consumer-contracts.md)'s mapping of it
([`../PRD.md`](../PRD.md) §6.7).

#### One ordering key

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-one-ordering-key-evt`

Commit + emission preserve order within the pinned `(orderingTenantId, subscriptionId)` (SUB-D-06) so rating consumes
composition changes without reorder hazards ([`../PRD.md`](../PRD.md) §6.7; SEAMS **SUB-R1**).

### 2.2 Constraints

#### Recurring is idempotent; posted is immutable

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-recurring-idempotent-evt`

At most one recurring `BillableItem` per **`(subscriptionId, billing period, lineKey)`** even under
bill-run retries — the key is **per billable component** (SUB-D-19, 2026-07-28 review fix, flagged
for veto): a subscription is a plan line plus N `AddOn` lines (slice [`02`](./02-composition-versioning.md)),
each with its own catalog keys, so a subscription-wide key could represent only one of them. Posted
invoice lines are never rewritten ([`../PRD.md`](../PRD.md) §6.8 AC 5).

#### Outbox is committed with the transition

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-transactional-outbox-evt`

Events are written to the outbox **in the same commit** as the state change (slice 01) — no event
without a committed transition, no committed transition without its events.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-domain-model-evt`

- **`LifecycleEvent`** — the producer envelope (type, identity, tenancy, correlation, time, sequence); CloudEvents 1.0, tenant-scoped, minimal PII.
- **`RecurringBillableItem`** — the recurring handoff keyed **`(subscriptionId, billing period, lineKey)`** with the per-component traceability tuple + `pricingSnapshotRef` (SUB-D-19).
- **`lineKey`** — the identifier of a billable **component interval** in the subscription's effective composition (SUB-D-21, 2026-08-01 — the value, occurrence dimension, and stability were previously undefined on both sides of the SUB-R6 seam): **`plan#n`** for the n-th `PlanLink` interval and **`addon:{addOnId}#n`** for the n-th interval of that add-on, where `n` is the component's **1-based lifetime interval ordinal** (slice [`02`](./02-composition-versioning.md) §4.1 — assigned at interval open, immutable). Stability rules: a multi-period interval keeps its `lineKey` across periods (the unique index yields one fact per period per interval); an in-place `changePlan` opens `plan#n+1`; an add-on re-added within one period gets a fresh ordinal (two facts, no key collision); `updateQuantity` opens **no** interval and changes no key; a cancel+new successor is a new `subscriptionId` with its own ordinals. It is the **same coordinate rating carries in its period-driven unit key** `(subscription, priceId, chargeKind, lineKey, AnchorPeriod)` (rating T-D-15) — rating must adopt this value rule into the SUB-R6/SB1 joint fixture — which is what lets SUB-D-07's "the priced line inherits the fact's key" hold for a multi-component subscription (SUB-D-19, flagged for veto).
- **`TraceabilityTuple`** — `{subscriptionId, skuId, planId, priceId}` + `pricingSnapshotRef`, **per component** — each emitted fact carries the catalog keys of its own line, not the plan's (SUB-D-19).

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-events-evt`

- **`EventPublisher`** — assembles the sufficient payload + writes to the outbox in the transition commit.
- **`OrderingSequencer`** (Foundation) — enforces per-`(orderingTenantId, subscriptionId)` order on emission (the pinned tenant, SUB-D-06).
- **`RecurringEmitter`** — cuts the **money-free recurring period facts**, one per billable component, idempotent per `(subscriptionId, billing period, lineKey)` (SUB-D-19) with the per-component traceability tuple + pause/intent posture; rating prices them (SUB-D-07, §4.3).
- **`OneTimeEmitter`** (SUB-D-24, 2026-08-01) — emits the **amount-less one-time/setup billable** at its qualifying instant (activation; trial conversion for a trialed plan — fired from those commits, not a clock), idempotent once per subscription lifetime per `(subscriptionId, priceId)` (§3.7); Billing values it from the frozen `pricingSnapshotRef` (SEAMS **SUB-B8**).

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-interface-events-evt`

The producer inventory + the recurring `BillableItemCreated` handoff are the outbound contract; this
slice fixes the producer set, the **event field matrix** (§4.1 — 2026-07-28 review fix: this
sentence had re-created the circular deferral SUB-D-09 closed by assigning the matrix to 09), and
the sufficiency/ordering/idempotency/traceability rules; the **CloudEvents wire extensions and the
Billing handoff wire payload** are owned by
[`09-consumer-contracts.md`](./09-consumer-contracts.md) as mappings of this slice's matrix.

### 3.4 Internal Dependencies

Depends on [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (transactional outbox,
`OrderingSequencer`); every capability slice (02–07) produces the events emitted here.

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|---------------------------|----------|
| Rating | Consumes composition-changing events on the shared ordering key | SEAMS **SUB-R1** |
| Billing | Consumes recurring `BillableItem`s; posts immutable invoices | SEAMS **SUB-B1** |
| OSS / Policy / Analytics | Consume lifecycle facts + confirmations | [`../PRD.md`](../PRD.md) §6.7 |
| Contracts | `PriceOverride` windows consumed into composition/renewal | SEAMS **SUB-C5** |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-flow-emit-evt`

**Emit**: on a committed transition, `EventPublisher` writes the sufficient-payload event(s) to the
outbox in the same commit; `OrderingSequencer` assigns the per-aggregate sequence; the outbox delivers
at-least-once, dedupable, in order within the pinned `(orderingTenantId, subscriptionId)`. `RecurringEmitter`
cuts one recurring `BillableItem` **per billable component**, idempotent per
`(subscriptionId, billing period, lineKey)`, each with its own traceability tuple +
`pricingSnapshotRef` (SUB-D-19).

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-storage-events-evt`

Uses the Foundation `event_outbox` (committed with the transition); the recurring idempotency key
**`(subscriptionId, billing period, lineKey)`** is a unique index on the recurring handoff record
(SUB-D-19 — the index is three-part so plan and add-on lines of one period coexist without
colliding), and the **one-time lifetime dedup `(subscriptionId, priceId)`** is a unique index on the
one-time handoff record (SUB-D-24, 2026-08-01 — the "once per subscription lifetime" key previously
had no table to live in). No further owned store. Concrete DDL is Design.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-deployment-evt`

Outbox delivery + the recurring-generation cut run as coordinated singletons **per tenant partition**
(one lease per `orderingTenantId` shard, shard-parallel — the same sharding rule as slice 04's jobs),
with the same **intra-tenant sub-sharding by hash of `subscriptionId`** (slice 04 §3.8) so a single
large tenant's daily 00:00 cut is not serialised through one worker, so the cut and the p95 < 30s
delivery target are not funnelled through one global instance
([`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) §3.8).

## 4. Additional Context

### 4.1 Producer Inventory and Payload Sufficiency (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-producers-evt`

- Subscriptions emits (CloudEvents 1.0, tenant-scoped, minimal PII): `SubscriptionCreated`, `SubscriptionActivated`, `SubscriptionSuspended` (**`reason ∈ {grace_driven, operator, policy}`** — registry-normative 2026-07-28: the §4.1 resume guard distinguishes grace-driven internally via `GraceLadderState`, consumers get the discriminator here), `SubscriptionResumed`, `SubscriptionCancelled` (**`reason ∈ {customer, operator, term_expired, nonpayment_exhausted, saga_superseded}`** — the §4.3a/SUB-D-16 system-derived exits carry their reasons; **payload additionally carries the current term window `termStartAt`/`termEndAt` + the billing-period identity containing the cancellation instant** — SUB-D-25, 2026-08-01: Billing's join key against the period facts it already holds, so ETF/credit derivation never re-reads the aggregate at posting time), `SubscriptionArchived` (the `cancelled → archived` retention edge, slice 01 — 2026-07-28 review fix: the outbox rule "no committed transition without its events" already implied it), `SubscriptionPlanChanged`, `BillableItemCreated` (**`kind ∈ {recurring, one_time}`** — the one-time lane added 2026-08-01, SUB-D-24; both kinds carry the identity/tenancy/correlation/time groups + the per-component traceability tuple + `pricingSnapshotRef`, the recurring kind adds the period key + postures + payer axis (§4.3), the one-time kind adds the **qualifying instant** and is **amount-less** like every artifact here), `EntitlementIssued`, `EntitlementRevoked`, `OwnershipTransferRequested`/`Approved`/`Completed` ([`../PRD.md`](../PRD.md) §6.7).
- **Secondary producer set (SUB-D-09 — naming is normative here; this closes the PRD's "naming per Design" obligations):**

  | Event | Source / AC |
  |-------|-------------|
  | `SubscriptionIntentScheduled` / `SubscriptionIntentUnscheduled` | Scheduled intents, slice 01 §4.3 (AC 22); un-schedule voids a previously announced boundary (slice 03 event-once convention). `SubscriptionIntentUnscheduled` carries **`reason ∈ {operator, firing_failed, saga_compensated, superseded_terminal, superseded_by_manual_change}`** (2026-07-28 review fix — the slice 01/03 discriminators are now registry-normative; consumers branch on it: `firing_failed` retracts a boundary, the rest void without one having fired) |
  | `SubscriptionQuantityChanged` | The composition-changing quantity event, slice 03 (AC 23; consumed like `SubscriptionPlanChanged`) |
  | `SubscriptionRenewalSucceeded` / `SubscriptionRenewalFailed` | Renewal job outcome, slice 04 §4.3 (AC 7) |
  | `SubscriptionGraceEntered` / `SubscriptionGraceExited` | Grace ladder, slice 04 §4.4 (AC 7; exit carries the resolution: renewed / suspended / cancelled) |
  | `SubscriptionRenewalNoticeDue` | Notice trigger, slice 04 §4.5 (AC 19; delivery = Notifications). Payload carries **`priceChangePending: bool`** (SUB-D-17 — derived from the SUB-P6 lookahead inputs, slice 09 §4.2; arms the commercial-notice variant) — carrier added 2026-08-01, wave-3 review #23 |
  | `SubscriptionCollectionPaused` / `SubscriptionCollectionResumed` | Pause window, slice 04 §4.2 (AC 24) |
  | `SubscriptionTrialConverted` / `SubscriptionTrialExtended` / `SubscriptionTrialExpired` | Trials, slice 06 (AC 16–18; `TrialExpired` doubles as the win-back hook; `TrialExtended` carries the moved boundary on the shared channel) |
  | `SubscriptionAcceptanceConfirmed` | `confirmAcceptance`, slice 01 §4.4 (AC 25) |
  | `EntitlementQuotaWarning` / `EntitlementQuotaExhausted` / `EntitlementQuotaRestored` | Quota crossings, slice 05 §4.4 (AC 14) |
  | `EntitlementRevoked.reason` | Registry-normative discriminator set: `transition` (posture change), `quantity_decrease` (slice 03 LIFO forced revocation), `policy` — 2026-07-28: the values were named in slice bodies with no registry home |
  | `EntitlementFrozen` / `EntitlementUnfrozen` | The slice 05 §4.1 first-class `frozen` state (suspend freezes / resume unfreezes) — 2026-07-28 review fix: consumers mirroring from `Issued`/`Revoked` alone read frozen entitlements as active; these carry the freeze posture explicitly |
  | `SeatBound` / `SeatReleased` | The slice 05 `bindSeat`/`releaseSeat` write pair (2026-07-28 billing-pass review #7 — "no committed transition without its events"; consumers track committed seat consumption without polling the guard's counter) |
  | `OwnershipTransferRejected` | The transfer flow's death, slice 07 — `reason ∈ {approval_denied, proof_expired_or_revoked, overlap_violation, guard_violation}`; covers both the approval denial and the commit-time abort (proof re-validation, overlap re-check), so a consumer that reacted to `Requested` always learns the outcome (2026-07-28 review fix) |
  | `SubscriptionTrialExtensionDenied` | `extendTrial` approval denial, slice 06 — the second approval-carrying type's denial outcome (2026-07-28 review fix) |
  | `SubscriptionRampHalted` | The slice 03 §4.5 mid-ramp halt — `reason ∈ {terminal_step_failure, superseded_by_manual_change}` (the manual-supersession value per SUB-D-15); names the previously anonymous "auditable failure event", carries the parked step set (`suspended pending re-authoring` = the halt posture of the remaining intents, cleared by Contracts re-authoring), and doubles as the Contracts re-author signal (2026-07-28 review fix) |

- **Sufficiency, not schema**: every lifecycle event carries enough identity/tenancy/correlation/time for route/dedup/replay **without** an undocumented side channel; composition-changing events carry enough snapshot-oriented commercial context that rating + Billing stay aligned on the effective offer and process idempotently (AC 11). The **field matrix is owned by this slice together with §4.1's registry** (per-event required-context groups: identity, tenancy incl. the pinned `orderingTenantId`, correlation, time, commercial snapshot context for composition changes); slice 09 owns only the wire mappings (SUB-D-09 closes the earlier circular deferral).

### 4.2 Ordering (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-ordering-evt`

- Order MUST be preserved within `(tenantId, aggregateId)` with `aggregateId = subscriptionId`; `tenantId` = the **pinned `orderingTenantId`** (= `resourceTenantId` at creation, immutable across transfers — SUB-D-06, AC 26) so subscription command ordering + downstream rating partition ordering share one **stable** key ([`../PRD.md`](../PRD.md) §6.7 AC 3; SEAMS **SUB-R1**).

### 4.3 Recurring Idempotency and No Retro-Edit (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-recurring-evt`

- `BillableItem(kind=recurring)` MUST be idempotent on `(subscriptionId, billing period, lineKey)` — at most one recurring item per key even under bill-run retries; the `lineKey` component dimension (SUB-D-19) is what lets a plan line and its add-on lines coexist in one period ([`../PRD.md`](../PRD.md) §6.8 AC 5).
- **What the emitter cuts is the money-free period fact (SUB-D-07):** period identity from the billing anchor, the traceability tuple, `pricingSnapshotRef`, the pause/intent posture, the **suspended interval(s) + suspension-billing posture** (2026-07-28 billing-pass review #3 — each suspension episode overlapping the period as `[suspendedAt, resumedAt)` clipped to the period, plus the explicit `pause_recurring | continue` policy: rating prorates the recurring line from these, this gear computes no money; without them neither gear could price a mid-period suspension), and the **`payerTenantId` in force at the period start** (2026-07-28 review fix, re-anchored 2026-08-01 by SUB-D-20 — the original "snapshotted at cut time" held only while cut time ≈ period start; a §4.3b revival cut can run up to 90 days later, and a transfer executed during the suspension must not land the consumed period on the new payer. The fact carries its payer axis explicitly, derived from committed transfer history: Billing posts the period to the fact's frozen payer, never to a payer re-resolved from the aggregate at posting time; this is what makes the slice-07 "in-flight period stays with the old payer" next-cycle default *representable*, not merely asserted) — **no monetary column** (the Foundation store has none). **Component enumeration is as-of the period, never as-of the run (SUB-D-20/21):** the cut emits one fact per component interval **overlapping the period**, read from the effective-dated composition over that period — deterministic whenever the cut runs, because the interval store's past is immutable. **The fact is the WHEN + identity trigger; coverage is derived:** rating resolves the interval in force / `quantity @ t` / the phase chain from the read model over the period (slice 02 §4.1 @-`t` discipline), so an interval closed early after its fact was cut under-covers automatically — fact immutability never overcharges. The **rating gear prices** the recurring component from the frozen snapshot and the priced line **inherits the fact's key** before Billing posts (AC 27; SEAMS **SUB-R6**). This removes the double-producer collision with rating's recurring lines.
- **Pause marker:** during a `collectionPaused` window the fact is still emitted, marked, so Billing owns the suppress-vs-defer treatment (SUB-D-03/12, AC 24 — "not posted" is Billing's act, emission is ours). This emit-and-mark rule never overrides grace: a next-term fact blocked by the grace ladder (design 04 §4.4) stays un-emitted through any pause window — grace governs **emission**, the pause defers **collection** of emitted facts only (precedence made explicit 2026-07-28).
- **Period-key stability:** the period identity is the anchor-derived canonical id frozen at emission; a cycle-length change starts a **new period sequence** at its boundary — no retroactive re-keying of already-cut facts. **Anchor derivation (SUB-D-27, 2026-08-01):** `[periodStart, periodEnd)` derives from the anchor instant (Contract terms, slice 01 §3.7) under the **frozen pricing `billingAnchorPolicy`** — the K2 enum adopted verbatim incl. the **D-20 no-drift month-end clamp** (a Jan-31 monthly anchor bills Feb-28/29 and returns to the 31st); an anchor-altering plan change takes effect at the **next** period boundary; rating consumes this identity (its T-D-33 fact-driven tick) and the **K5 joint anchor fixture** asserts identity ≡ rating's calendar geometry — a design-freeze gate. (2026-07-28 review #18: the in-place path that legitimately changes cycle length is a **Contract term change taking effect at renewal** — same plan, new `Renewal` terms; a customer-initiated cross-frequency `changePlan` is **cancel+new** per slice 03 §4.3, whose successor starts its own sequence anyway. The rule covers both.)
- **Cut-vs-intent race:** the daily cut reads the pending-intent set as of its run; an `unschedule` committed after the cut suppressed a period re-triggers a targeted re-cut via `SubscriptionIntentUnscheduled` (idempotent on the same key), with the §17.1 charge-coverage reconciliation as the backstop.
- **Mid-period interval open (SUB-D-21, 2026-08-01 wave-3 review #2):** an interval **opened mid-period** — an immediate `addAddOn`, an in-place immediate `changePlan` — gets a **targeted cut in its opening commit**: the fact for `(subscriptionId, current period, new lineKey)` emits with the transition, and rating prorates the new interval's stretch from the read model. This is the producer slice 03's "Immediate ⇒ delta recurring" had been missing since SUB-D-07 made the fact the only recurring trigger. `updateQuantity` opens no interval and triggers no cut — the delta rides the existing fact (rating prorates from `quantity @ t` + the `SubscriptionQuantityChanged` boundary); an interval **closed** mid-period triggers nothing — the standing fact under-covers via derivation, and any commercial remainder is the SUB-D-18/25 Contracts/Billing lane, never a retraction.
- **Terminal-before-cut (SUB-D-21, wave-3 review #15):** the daily cut's **status precondition** is defined by the period, not the live status: a period the term **had begun serving** (period start < the terminal instant) is still cut — even if the subscription is already `cancelled` when the cut runs — and rating clips the served stretch to the intervals the terminal sweep closed (slice 01 §4.3); a period starting **at or after** the terminal instant is never cut. So a cancel between period start and the cut neither silently skips the served days nor bills a full period never served; the same period-anchored rule governs §4.2a's deferred collection of a window a terminal commit closed.
- **Trial phases are cut normally (SUB-D-21, wave-3 review #16):** the emitter does not distinguish a trial phase — the period fact emits with the same key discipline, and **rating prices the phase from the frozen snapshot's phase chain** (a free trial phase resolves to its zero-amount phase row; the phase shape is pricing's, D-40/D-41). `convertTrial` **moves the phase boundary, not the period identity**: no new period sequence starts at conversion (the anchor governs; the new-sequence triggers stay exactly cycle-length change and cancel+new), the boundary travels on `SubscriptionTrialConverted`, and rating splits the period's plan line at it — so a mid-period conversion's first paid stretch is priced from the already-cut period fact, never lost. The one-time conversion charge is the separate SUB-D-24 lane.
- **Quota-cycle reset is period-keyed (SUB-D-21, wave-3 review #9):** the slice 05 §4.4 counter reset is idempotent per `(subscriptionId, billing period)` — fired by the period's **first** cut, a no-op on every later component cut and on the targeted re-cut — since SUB-D-19 made "the cut" plural per period.
- **One-time / setup charges are emitted here, not rated (T-D-18 adoption, 2026-07-28 — formalised as SUB-D-24, 2026-08-01, flagged for veto):** rating synthesizes **no evaluation unit** for `chargeKind ∈ {one_time, one_time_setup}` rows (rating T-D-18; its three unit kinds are exhaustive without them), so this gear emits the one-time billable **at the qualifying instant** — subscription activation, or **trial conversion** (first non-trial phase entry) for a trialed plan — on the same at-sale path as a commitment sale (T-D-14). The emitted fact is **amount-less** (the money-free doctrine stays absolute, not recurring-scoped): `BillableItemCreated(kind=one_time)` carries the qualifying instant, the per-component traceability tuple, and the frozen `pricingSnapshotRef`; **Billing values it from the ref** (SEAMS **SUB-B8** — copying a frozen flat amount is resolving a published price fact; pricing forbids tier machinery on one-time rows). The **once-per-subscription-lifetime dedup** is owned here: the `(subscriptionId, priceId)` unique index (§3.7), so re-activation, resume, plan change, and `PlanLink` migration never re-emit it; a conversion charge blocked by a payment failure emits on the §4.3b payment-resolved signal (slice 06 §3.6). Emitter = `OneTimeEmitter` (§3.2); wire payload = slice 09 §4.3. One-time rows still resolve in catalog step-2 selection for coverage/preview/quote — selection without unit synthesis.
- Posted invoice lines MUST NOT be rewritten; subscription corrections emit **new** billable or adjustment paths ([`../PRD.md`](../PRD.md) §6.8; SEAMS **SUB-B1**).
- **Mid-term cancellation disposition (SUB-D-18, 2026-07-28 billing-pass review #10; reason-scoped by SUB-D-25, 2026-08-01):** a `cancel` landing **inside** an already-cut period does **not** retract the fact — the period was contracted and the fact/posted line stand (immutability). The commercial consequence (early-termination fee, refund, or credit) is **Contracts-defined and Billing-materialised**: `SubscriptionCancelled` carries the **cancellation instant, `cancelMode`, reason, the contract ref, the current term window, and the containing billing-period identity** (§4.1 registry), and Billing derives the ETF/credit artifacts from the Contract terms as **new** artifacts, joining on the term/period identities the event carries — never re-deriving the period from the aggregate at posting time. **The derivation applies only to the early-termination reason class — `customer` and `operator`** (Contracts defines the amounts *within* it); **`term_expired`, `nonpayment_exhausted`, and `saga_superseded` never derive an ETF or unused-portion credit** — a natural term end has no early termination, involuntary nonpayment churn is the collections/write-off path (§4.2a's deferred collection still runs; no fee on top of the debt), and a saga-superseded predecessor's "cancellation" is a plan change whose successor's contract governs. This gear computes no money and emits no adjustment itself (ownership row SEAMS **SUB-B7**).

### 4.4 Traceability (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-traceability-evt`

- Items MUST trace **per component**: each item carries `subscriptionId`, its `lineKey`, and **its own component's** `{skuId, planId, priceId}` + `pricingSnapshotRef` (manifest §4.4 itemization; SUB-D-19 — re-scoped 2026-08-01, wave-3 review #7: the pre-SUB-D-19 singular tuple here would have stamped the plan's catalog keys on add-on lines) — the charge-to-catalog lineage partners + auditors reconcile against ([`../PRD.md`](../PRD.md) §6.8).

### 4.5 Dataset Separation and PriceOverride Consumption (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-normative-dataset-priceoverride-evt`

- Subscription state ≠ invoice posted state; late usage adjustments remain a Rating→Billing concern; this slice states **Billing invariants** for coordinated artifacts, not client control-plane operations (REST paths/methods/errors are Design) ([`../PRD.md`](../PRD.md) §6.8).
- Contract `PriceOverride` windows are consumed into composition/renewal via events/read models; in rating these are the step-5 contract overlay — Subscriptions references the binding, never evaluates the override (SEAMS **SUB-C5**).

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §6.7 (`fr-event-producers`, `fr-event-payload-completeness`, `fr-event-consumers`, `fr-event-ordering`), §6.8 (`fr-recurring-idempotency`, `fr-no-retro-edit`, `fr-billing-traceability`, `fr-dataset-separation`), AC 3/5/11, §7.1 (delivery NFR).

**Traces to**: `cpt-cf-bss-subscriptions-fr-event-producers`, `cpt-cf-bss-subscriptions-fr-event-payload-completeness`, `cpt-cf-bss-subscriptions-fr-event-consumers`, `cpt-cf-bss-subscriptions-fr-event-ordering`, `cpt-cf-bss-subscriptions-fr-recurring-idempotency`, `cpt-cf-bss-subscriptions-fr-no-retro-edit`, `cpt-cf-bss-subscriptions-fr-billing-traceability`, `cpt-cf-bss-subscriptions-fr-dataset-separation` *(single-owner FR claims — the P2 traceability convention adopted 2026-08-01, wave-3 review #24h; shared mechanics other slices cite stay narrative, each FR has exactly one owning slice)*
- **Seams**: **SUB-B1** (recurring/immutability/traceability), **SUB-R1** (ordering), **SUB-R6** (fact-key inheritance), **SUB-B7** (cancellation money path), **SUB-B8** (one-time valuation), **SUB-C5** (PriceOverride) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions** (2026-08-01, wave-3 review #24c — this slice had no Decisions bullet despite being the propagation target): SUB-D-07 (money-free recurring split; §4.3), SUB-D-09 (secondary event registry; §4.1), SUB-D-18 (mid-term cancellation disposition; §4.3), SUB-D-19 (per-component key; §3.1/§3.7/§4.3), SUB-D-20 (as-of enumeration + payer axis; §4.3), SUB-D-21 (`lineKey` identity + emitter completeness; §3.1/§4.3), SUB-D-24 (one-time lane; §3.2/§3.7/§4.1/§4.3), SUB-D-25 (ETF reason class + join key; §4.1/§4.3), SUB-D-27 (anchor policy + clamp; §4.3) — [`../DECISIONS.md`](../DECISIONS.md).
- **Slices**: [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (outbox, sequencer), capability slices 02–07 (event sources), [`09-consumer-contracts.md`](./09-consumer-contracts.md) (payload + Billing handoff contract).
