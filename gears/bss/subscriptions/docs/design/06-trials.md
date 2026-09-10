Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Subscriptions — Trial Runtime & Conversion (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (trial offer, phase, convertsToPhaseId, grant set), Payments (conversion authorization) | Downstream: Rating (phase boundary), Notifications (win-back) | Owners: BSS Subscriptions team -->

# DESIGN — Trial Runtime & Conversion (Slice 6)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-design-trials`

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
  - [4.1 Trial as a Phase, Not a Status (normative)](#41-trial-as-a-phase-not-a-status-normative)
  - [4.2 End-of-Trial Conversion (normative)](#42-end-of-trial-conversion-normative)
  - [4.3 Early Conversion — convertTrial (normative)](#43-early-conversion--converttrial-normative)
  - [4.4 Expiry Without Conversion (normative)](#44-expiry-without-conversion-normative)
  - [4.5 Trial Extension (normative)](#45-trial-extension-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice runs the trial **on the phase machinery** — never a status fork. A trial is the leading
**plan phase**; the subscription occupies a manifest status (`draft` before first paid activation,
`active` under trial service) while trial state lives as attributes + `PlanLink`/snapshot pointers
([`../PRD.md`](../PRD.md) §6.1, §6.10). Conversion — at term via `convertsToPhaseId`, or early via
`convertTrial` — advances the phase boundary, an instant the rating gear consumes exactly like a
`changeEffectiveAt`; entitlements re-issue per the target phase's grant set with **no access gap**.

Two seams meet here: **SUB-R4** (the phase boundary = change boundary, travelling on the shared
`(changeEffectiveAt, changeMode)` channel) and **SUB-P3** (the trial sellable definition is
Catalog-authored).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-subscriptions-fr-trial-provisioning` / `cpt-cf-bss-subscriptions-fr-trial-commercial-pattern` | A trial is created from a Catalog-defined trial offer (trial plan/SKU or a leading trial phase); trial state is evaluated attributes + `PlanLink`/snapshot pointers; feature access follows the trial-phase grant set (§4.1). |
| `cpt-cf-bss-subscriptions-fr-trial-conversion` | At trial end, convert per `convertsToPhaseId`: advance the phase boundary, authorize payment where required, re-issue entitlements with continuity, emit the composition-changing event; idempotent (zero missed / zero double) (§4.2). |
| `cpt-cf-bss-subscriptions-fr-trial-early-conversion` | `convertTrial` is a first-class `TransitionRequest` advancing the boundary to `now` (the phase twin of `changePlan`), Policy-gated where resource-affecting, idempotent (§4.3). |
| `cpt-cf-bss-subscriptions-fr-trial-expiry` | An unconverted trial follows the configured end action via normal transitions (typically `cancel`); entitlements removed; optional win-back hook emitted (§4.4). |
| `cpt-cf-bss-subscriptions-fr-trial-extension` | An approval-gated operation moving the conversion date + trial-phase end consistently, with audit (§4.5). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-subscriptions-nfr-operational-baselines` | Conversion job | Idempotent conversion (zero missed / zero double); reconciliation §17.1 trial-conversion check | Reconciliation + fixtures |
| `cpt-cf-bss-subscriptions-nfr-lifecycle-latency` | `convertTrial` commit path | Synchronous commit class (p95 < 1s) = the **intent commit** — a conversion from `draft` is an `activate` (OSS-blocking), so its status commit is async and outside the bound (slice 01 NFR row is the authority) | Load test |

#### Key ADRs

No slice-local ADR; the trial-as-phase modelling is governed by the closed-status-machine ADR (slice
01) and SEAMS **SUB-P3**/**SUB-R4**.

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-tech-stack-trl`

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Trial provisioning, conversion (term + early), expiry, extension | Rust module in the `subscriptions` gear |
| Domain | Trial phase state, `convertsToPhaseId` boundary, conversion + extension records | Rust; GTS + Rust domain structs |
| Infrastructure | Trial phase-state table; conversion job via the lease library | PostgreSQL, SecureORM |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Trial is a phase, not a status

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-trial-phase-trl`

A trial is the leading plan phase on a manifest status; there is no `trial` status or edge. Trial
state is attributes + `PlanLink`/snapshot pointers ([`../PRD.md`](../PRD.md) §6.1; slice 01 closed
enum).

#### Conversion is continuous

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-conversion-continuity-trl`

Conversion re-issues entitlements per the target phase with **no access gap** and is **idempotent** —
zero missed, zero double conversions ([`../PRD.md`](../PRD.md) §6.10).

### 2.2 Constraints

#### Catalog owns the trial definition

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-catalog-trial-trl`

The trial sellable definition (trial plan/SKU, leading trial phase) is
Catalog-authored; attribute-only trials are permitted only when Contract records the trial commercial
terms (SEAMS **SUB-P3**; [`../PRD.md`](../PRD.md) §6.1).

#### Phase boundary rides the shared channel

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-phase-channel-trl`

A conversion advances the phase boundary as a `(changeEffectiveAt, changeMode)` the rating gear
consumes like any change boundary — no second boundary vocabulary (SEAMS **SUB-R4**).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-domain-model-trl`

- **`TrialPhaseState`** — the active trial phase (`phase_id`), duration, `convertsToPhaseId`, evaluated trial attributes.
- **`ConversionRecord`** — the conversion event (term or early), boundary instant, payment-authorization outcome, entitlement re-issue reference; idempotency key.
- **`TrialExtension`** — an approval-gated move of the conversion date + trial-phase end (with `Approval`).

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-trials-trl`

- **`TrialProvisioner`** — creates the trial from the Catalog offer on the phase machinery.
- **`ConversionEngine`** — the term (`convertsToPhaseId`) + early (`convertTrial`) conversion; advances the phase boundary, drives payment authorization, re-issues entitlements (slice 05).
- **`TrialExpiryHandler`** — the end action for an unconverted trial (typically `cancel`) + the win-back hook.
- **`TrialExtensionHandler`** — the approval-gated extension.

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-interface-trial-trl`

`convertTrial` is a first-class operation (`TransitionRequest.type`, pending manifest alignment, slice
01 §4.6); trial provisioning + extension are operations with the standard idempotency/approval rules.
The conversion emits a composition-changing event consumed like `SubscriptionPlanChanged`. Wire
mappings + event fields are owned by [`08-events-billing.md`](./08-events-billing.md) /
[`09-consumer-contracts.md`](./09-consumer-contracts.md).

### 3.4 Internal Dependencies

Depends on [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (`convertTrial` transition,
`Approval` for extension), [`02-composition-versioning.md`](./02-composition-versioning.md) (phase
intervals), and [`05-entitlements.md`](./05-entitlements.md) (per-phase grant re-issue). Payment
failure at conversion follows [`04-suspension-renewal-grace.md`](./04-suspension-renewal-grace.md)'s
grace ladder.

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|---------------------------|----------|
| Pricing | Trial offer, `convertsToPhaseId`, per-phase grant set | SEAMS **SUB-P3**, **SUB-P2** |
| Payments | Conversion authorization (method on file) | SEAMS **SUB-F1** |
| Rating | Consumes the phase boundary as a change boundary | SEAMS **SUB-R4** |
| Notifications | Win-back hook delivery (trigger owned here) | SEAMS **SUB-F2** |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-flow-trial-conversion-trl`

**Conversion** (refines `cpt-cf-bss-subscriptions-seq-trial-conversion`): **(0) pending-intent check
(2026-07-28 review fix)** — the conversion flow **reads the pending-intent set first**, exactly like
the `RenewalJob` (slice 04 §4.3): a pending `cancel` scheduled at/before the trial boundary — the most
common trial intent — **suppresses the conversion** (no boundary advance, no paid-phase entitlement
issuance, no conversion charge); the cancel fires instead, and if its firing fails the slice-01
firing-failure taxonomy governs (a retryable failure retries with the conversion suppression **held**;
a terminal failure parks with an operator alarm and the conversion stays suppressed — the customer who
asked to cancel at trial end is never converted and charged because a Policy blip ate the cancel) →
advance the phase boundary
(`convertsToPhaseId` at term, or `convertTrial` to `now`) → authorize payment where required (method
on file, no re-entry) → re-issue entitlements per the target phase's grant set with no access gap →
emit `SubscriptionTrialConverted` (the composition-changing conversion event, SUB-D-09); idempotent
(zero missed / zero double). A `suspended` subscription at the boundary parks the conversion in the
**state-precondition class** (SUB-D-23 — the deadline suspends while `suspended` persists and the
resume commit re-arms it) until resume or terminal (slice 01 §4.2 source-status matrix / §4.3 taxonomy).

**Payment failure at conversion (2026-07-15 review fix):** the boundary advance is **not rolled
back** — the phase boundary moves at its scheduled instant regardless (deterministic for rating), the
**target-phase entitlements are issued** (AC 16 continuity — the customer is on the paid phase, with
paid access), and the failed conversion charge (the SUB-D-24 one-time lane) enters the §6.5 grace ladder as its blocked collection;
grace failure exits to `suspended`/`cancelled` per the Contract ladder. Access continuity is never
traded for collection state — the grace ladder, not a grant rollback, is the pressure mechanism.
**Revival after a conversion-driven suspension (SUB-D-20, 2026-08-01 wave-3 review #11):** there is
no never-succeeded renewal to re-run — the paid phase and its term are already in force (the boundary
advance is never rolled back) — so slice 04 §4.3b **degenerates to payment resolution → `resume`**,
with the blocked one-time conversion billable emitting on the payment-resolved signal; only if the
term also expired during the suspension does the full backdated-extension ordering apply (to that
renewal). Before this rule the mandated middle step (term extension) had no object and the resume
precondition could never be satisfied — a paying customer stayed suspended until the dwell cancelled
them.

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-storage-trial-trl`

Owned here: `trial_phase_state` (active phase, duration, `convertsToPhaseId`) and `conversion_record`
(idempotency-keyed). Trial attributes ride the aggregate; extension uses the Foundation `approval`.
Concrete DDL is Design.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-deployment-trl`

The term-conversion job runs as a coordinated singleton via the lease library; `convertTrial` is a
synchronous control-plane transition ([`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md)
§3.8).

**No double conversion (2026-07-28 review fix, REVIEW F-06-3, flagged for veto):** the two paths are
made mutually exclusive by a **state guard, not by a shared idempotency key** — the term-conversion
job **re-reads the phase state inside its commit** and is a **no-op when the trial phase is no longer
active** (already converted by an early `convertTrial`, or the subscription left `active`); symmetrically
`convertTrial` fails closed (`guard_violation`) if the boundary has already advanced. The client
`(subscriptionId, idempotencyKey)` key on `convertTrial` and the job's own scheduling key are
deliberately **not** coordinated — they key different actors, so only the phase state can arbitrate;
the `ConversionRecord` remains the single audited outcome either way.

## 4. Additional Context

### 4.1 Trial as a Phase, Not a Status (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-trial-phase-trl`

- A trial subscription is created from a **Catalog-defined trial offer** (trial plan/SKU or a leading trial **phase**) with configurable duration; **no `trial` status** — evaluated attributes + `PlanLink`/snapshot pointers persist while the subscription occupies a manifest status ([`../PRD.md`](../PRD.md) §6.1, §6.10; SEAMS **SUB-P3**).
- Feature access during trial follows the **trial-phase grant set** (slice 05 assignment).

### 4.2 End-of-Trial Conversion (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-conversion-trl`

- At trial end, convert per the plan's phase schedule (`convertsToPhaseId`): advance the phase boundary, authorize payment where required (Payments; no re-entry where a method is on file), re-issue entitlements per the target phase with **continuity** (no access gap), emit the composition-changing event ([`../PRD.md`](../PRD.md) §6.10).
- Conversion is **idempotent** — zero missed / zero double; a payment failure follows the §6.5 grace ladder.
- **No method on file (2026-07-15 review fix):** where the target phase requires payment and **no method is on file**, the conversion is treated as an **immediate payment failure** — the boundary still advances and target-phase entitlements are issued (the §4.2/§3.6 continuity rule: access is never traded for collection state), and the missing-method failure enters the §6.5 grace ladder as its blocked collection, driving the pre-suspension notice path (a prompt to add a method) rather than a silent conversion. Grace start for a first-time trial→paid conversion is the conversion instant (there is no prior term), so the ladder is well-defined without a renewal term.

### 4.3 Early Conversion — convertTrial (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-convert-trial-trl`

- `convertTrial` ("skip the trial, start paying now") is a first-class `TransitionRequest` advancing the phase boundary to `now` — the **phase twin of `changePlan`**, the boundary instant consumed by rating like any `changeEffectiveAt`; re-issues entitlements per the target phase, emits `SubscriptionTrialConverted`; Policy-gated where resource-affecting, idempotent on `(subscriptionId, idempotencyKey)` ([`../PRD.md`](../PRD.md) §6.10; gap **G-2**, SEAMS **SUB-R4**). **Period identity is untouched (SUB-D-21, 2026-08-01 wave-3 review #16):** conversion — scheduled or early — moves the **phase boundary, never the period sequence**; trial-phase periods are cut like any other (rating prices the phase from the frozen snapshot's phase chain, a free phase to its zero-amount row), so a mid-period conversion's first paid stretch is priced from the already-cut period fact split at the boundary this event carries. The one-time conversion charge is the SUB-D-24 lane, qualified at the conversion instant.
- **From `draft`:** a `convertTrial` on a never-activated trial is an **activate** directly into the target phase (one committed transition, activation guards apply); the trial phase is skipped, not exited.
- This extends the manifest §4.3 `TransitionRequest.type` list — manifest alignment tracked in slice 01 §4.6 / [`../PRD.md`](../PRD.md) §15. An unconverted trial expiring while still `draft` exits via the `draft → cancelled` void edge (SUB-D-11).

### 4.4 Expiry Without Conversion (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-expiry-trl`

- An unconverted trial at expiry follows the configured end action via **normal transitions** (typically `cancel`; never a bespoke terminal status): entitlements removed, auditable events emitted, and an optional **win-back hook** published — campaign content + delivery = Notifications/Comms (out of scope §5.2) ([`../PRD.md`](../PRD.md) §6.10; SEAMS **SUB-F2**).

### 4.5 Trial Extension (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-normative-extension-trl`

- A trial extension is the **approval-gated** `extendTrial` transition (SUB-D-08; Approval per the slice-01 high-risk pattern) moving the conversion date + trial-phase end consistently, with audit ([`../PRD.md`](../PRD.md) §6.10).
- **The moved boundary is announced to rating (2026-07-15 review fix):** the extension emits `SubscriptionTrialExtended` carrying the new phase-boundary instant on the shared `(changeEffectiveAt, changeMode)` channel (SEAMS **SUB-R4**) — the subscription-side phase state, not the snapshot-frozen phase schedule, is what rating resolves @ `t`, so the move needs no snapshot re-seal.
- **Open (§15):** the approval policy (automatic / manual / threshold-based) is a Product decision. Repeat-trial eligibility (serial re-trials) is an open Product/Pricing question ([`../PRD.md`](../PRD.md) §15) — the overlap rule blocks only concurrent duplicates.

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §6.10 (`fr-trial-provisioning`, `fr-trial-conversion`, `fr-trial-early-conversion`, `fr-trial-expiry`, `fr-trial-extension`), §6.1 (`fr-trials-not-a-status`, `fr-trial-commercial-pattern`), §7.1 (NFRs), §15 (extension-policy open), §17.1 (trial-conversion reconciliation).

**Traces to**: `cpt-cf-bss-subscriptions-fr-trial-commercial-pattern`, `cpt-cf-bss-subscriptions-fr-trial-provisioning`, `cpt-cf-bss-subscriptions-fr-trial-conversion`, `cpt-cf-bss-subscriptions-fr-trial-early-conversion`, `cpt-cf-bss-subscriptions-fr-trial-expiry`, `cpt-cf-bss-subscriptions-fr-trial-extension` *(single-owner FR claims — the P2 traceability convention adopted 2026-08-01, wave-3 review #24h; shared mechanics other slices cite stay narrative, each FR has exactly one owning slice)*
- **Seams**: **SUB-R4** (phase boundary), **SUB-P3** (trial offer); consumes **SUB-P2** (per-phase grant), **SUB-F1** (conversion auth) — [`../SEAMS.md`](../SEAMS.md).
- **Slices**: [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (`convertTrial`, approval), [`02-composition-versioning.md`](./02-composition-versioning.md) (phase intervals), [`04-suspension-renewal-grace.md`](./04-suspension-renewal-grace.md) (payment-failure grace), [`05-entitlements.md`](./05-entitlements.md) (per-phase re-issue), [`08-events-billing.md`](./08-events-billing.md) (conversion event).
