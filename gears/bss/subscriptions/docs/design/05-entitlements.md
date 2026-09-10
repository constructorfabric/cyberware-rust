Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Subscriptions — Entitlement Lifecycle (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (grant-set templates, incl. per-phase map), Rating (usage aggregates) | Downstream: OSS (enforcement), Billing/Rating (prepaid drawdown) | Owners: BSS Subscriptions team -->

# DESIGN — Entitlement Lifecycle (Slice 5)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-design-entitlements`

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
  - [4.1 Issue / Revoke from Transitions (normative)](#41-issue--revoke-from-transitions-normative)
  - [4.2 Assignment from the Grant Set (normative)](#42-assignment-from-the-grant-set-normative)
  - [4.3 Point-of-Use Check and Enforcement Split (normative)](#43-point-of-use-check-and-enforcement-split-normative)
  - [4.4 Quota Soft/Hard Limits (normative)](#44-quota-softhard-limits-normative)
  - [4.5 Prepaid Grant Hook (deferred)](#45-prepaid-grant-hook-deferred)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice makes entitlement posture a **deterministic function of committed subscription state**:
on every resource-affecting transition and phase boundary it issues or revokes entitlements from the
plan's **published grant set**, and it serves a real-time **point-of-use check decision state** that
OSS enforces. Subscriptions is the entitlement **SoR** (what is granted, quota remaining, limit
state); it **never executes enforcement** ([`../PRD.md`](../PRD.md) §6.9). The check surface is the
predecessor's core promise, held to **p95 < 100ms** ([`../PRD.md`](../PRD.md) §7.1).

Three seams meet here: **SUB-P2** (assignment from the pricing grant set incl. the per-phase map,
D-41), **SUB-E3** (the check-state ↔ OSS-enforcement split + the quota mid-request open), and
**SUB-B4** (the prepaid-credit hook — definition pricing D-43, drawdown Billing/Rating, GA-gated).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-subscriptions-fr-entitlement-issue-revoke` | On a successful resource-affecting transition (activate/suspend/resume/cancel/gated change) issue or revoke entitlements to match the new posture + emit `EntitlementIssued`/`EntitlementRevoked` (§4.1). |
| `cpt-cf-bss-subscriptions-fr-entitlement-assignment` | On activation, phase boundary, and committed change, assign from the plan's published grant set — incl. the **per-phase map** where phased (pricing D-41) — with effective dates aligned to `changeMode` (§4.2). |
| `cpt-cf-bss-subscriptions-fr-entitlement-check-contract` | A real-time check read contract (flag decision, quota remaining, limit state) at p95 < 100ms, tenant-isolated + cache-friendly; OSS enforces, this gear serves the decision state (§4.3). |
| `cpt-cf-bss-subscriptions-fr-entitlement-quota-limits` | Track usage vs quotas (fed by the rating pipeline); soft limit ⇒ auditable warning; hard limit ⇒ blocking check state; exhaustion/restore evented (§4.4). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-subscriptions-nfr-entitlement-check-latency` | Check read surface | Cache-first projection, tenant-isolated, single indexed read off the hot path — p95 < 100ms | Load test before GA ([`../PRD.md`](../PRD.md) §16) |
| `cpt-cf-bss-subscriptions-nfr-operational-baselines` | Assignment propagation | Entitlement update reaches the check surface < 5s | Reconciliation §17.1 (entitlement sync) |

#### Key ADRs

No slice-local ADR; the assignment source is governed by SEAMS **SUB-P2** (catalog authors, this gear
assigns) and the enforcement split by **SUB-E3**.

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-tech-stack-ent`

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Presentation | The point-of-use check read surface behind the gateway; admin issue/revoke paths | Rust, REST/OpenAPI |
| Application | Assignment from grant sets; issue/revoke on transitions; quota tracking | Rust module in the `subscriptions` gear |
| Domain | `Entitlement` state (flags/quotas/limits), grant-set resolution, quota counters | Rust; GTS + Rust domain structs |
| Infrastructure | Entitlement state store + the projected check read model | PostgreSQL, SecureORM |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Posture is a function of committed state

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-deterministic-posture-ent`

Entitlement posture is a deterministic function of committed subscription state; every grant/revoke
is a consequence of a committed transition, evented + audited ([`../PRD.md`](../PRD.md) §6.9).

#### Serve the decision, never enforce

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-serve-not-enforce-ent`

Subscriptions serves the decision state (allow/quota/limit); **OSS enforces** (allow/block/degrade).
This gear never executes enforcement ([`../PRD.md`](../PRD.md) §6.9; SEAMS **SUB-E3**).

### 2.2 Constraints

#### Assignment source is the catalog grant set

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-grant-set-source-ent`

Entitlements are assigned from the pricing-published grant set (incl. the per-phase map); this gear
authors no template — it resolves + materialises per subscription (SEAMS **SUB-P2**).

#### Quota state is commercial state

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-quota-commercial-ent`

Quota state is auditable commercial state, not an OSS side effect; crossings + exhaustion/restore
emit auditable events — never a silent overrun ([`../PRD.md`](../PRD.md) §6.9).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-domain-model-ent`

- **`Entitlement`** — a granted feature flag / usage quota / resource quota on a subscription, with effective dates and source grant-set reference.
- **`GrantSetAssignment`** — the resolved grant set (incl. the active phase's map) materialised per subscription at a transition.
- **`SeatBinding`** (2026-07-28 review fix — the seat-scoped entity the slice-03 quantity-decrease guard reads) — one committed seat unit of a quantity-bearing subscription: `(subscriptionId, seatBindingId)`, `state ∈ {active, frozen, revoked}`, bound/released **through the check-surface write API** by the consuming system, transactionally in this gear (per-user seat *placement* stays OSS-side — SUB-E3; this record is only the gear-committed consumption count). `COUNT(active)` is the guard's "quantity already consumed" — local and transactionally consistent by construction.
- **`QuotaCounter`** — usage-vs-quota tracking (fed by the rating pipeline), soft/hard thresholds, exhaustion/restore state.
- **`CheckDecision`** — the read-surface value object: flag decision, quota remaining, limit state. A **`frozen` grant answers deny/`blocking`** (2026-07-28 billing-pass review #8 — the authoritative surface must not read frozen as active: paid-feature access must not survive a nonpayment suspension; the registry events `EntitlementFrozen`/`Unfrozen` exist for mirrors, this sentence is the rule for the surface itself).

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-entitlements-ent`

- **`EntitlementAssigner`** — resolves the grant set (per-phase where phased) and materialises assignments at activation / phase boundary / committed change.
- **`IssueRevokeHandler`** — issues/revokes to match the posture on each resource-affecting transition; emits the producer events.
- **`QuotaTracker`** — folds rating-pipeline usage into `QuotaCounter`s; fires soft/hard + exhaustion/restore events.
- **`CheckReadSurface`** — serves the tenant-isolated `CheckDecision` at p95 < 100ms.

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-interface-entitlement-check-ent`

The **point-of-use check** read contract (feature-flag decision, quota remaining, limit state) is
owned here — tenant-isolated, cache-friendly, p95 < 100ms; OSS calls it and enforces. The internal
admin `issueEntitlement`/`revokeEntitlement` paths are audit-mandatory. The surface additionally
owns the **seat bind/release write pair** (`bindSeat`/`releaseSeat` — 2026-07-28 billing-pass
review #7: the operations `SeatBinding` rows are "bound/released through"; idempotency-keyed,
audited, rejected above the committed quantity fail-closed) — the consuming system calls them as
it places/frees seats; they mutate only this gear's `seat_binding` rows, never OSS state. Each
committed bind/release emits **`SeatBound`** / **`SeatReleased`** (slice 08 §4.1 registry — named
here 2026-08-01, wave-3 review #24g: the owning slice had never named the events its writes emit). Wire mappings + the external
OSS contract are refined in [`09-consumer-contracts.md`](./09-consumer-contracts.md).

### 3.4 Internal Dependencies

Depends on [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (issue/revoke at commit),
[`02-composition-versioning.md`](./02-composition-versioning.md) (active plan/phase @ `t`), and
[`06-trials.md`](./06-trials.md) (per-phase re-issue on conversion). Feeds
[`08-events-billing.md`](./08-events-billing.md) (entitlement events).

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|---------------------------|----------|
| Pricing | Grant-set templates incl. per-phase map (D-41) | SEAMS **SUB-P2** |
| Rating | Usage aggregates feeding quota counters | [`../PRD.md`](../PRD.md) §6.9 |
| OSS | Calls the check surface + enforces the decision | SEAMS **SUB-E3** |
| Billing / Rating | Prepaid-credit balance/drawdown (GA-gated) | SEAMS **SUB-B4** |

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-flow-entitlement-check-ent`

**Point-of-use check** (refines `cpt-cf-bss-subscriptions-seq-entitlement-check`): OSS reads the
`CheckReadSurface` for `(subscription, feature/quota)` → returns flag decision + quota remaining +
limit state at p95 < 100ms → OSS enforces (allow/block/degrade). Committed transitions update the
surface within the < 5s propagation baseline; quota crossings emit soft/hard + exhaustion/restore
events.

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-storage-entitlement-ent`

Owned here: `entitlement` (state + effective dates + grant-set source), **`seat_binding`**
(2026-07-28 billing-pass review #7 — the guard's `COUNT(active)` is transactionally consistent
only if the rows live here: `(subscription_id, seat_binding_id)`, `state`, bound/released
instants, partial index on `state = 'active'` per subscription), `quota_counter`, and the
projected `entitlement_check_read_model` (cache-first, tenant-isolated). Tenant-partitioned. Concrete
DDL is Design.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-deployment-ent`

The check read path is served from the projected read model for the p95 < 100ms target with the
**bounded-staleness degraded mode** (SUB-D-10, as amended): on projection outage the **feature-flag
dimension** serves the last-known-good decision up to the staleness budget (default 60s —
Product/OSS knob, [`../PRD.md`](../PRD.md) §15) then fails closed, while the **quota dimension**
fails closed to `blocking` beyond its own tighter freshness bound (provisional 10s) and never
serves a last-known-good `allow` (the §4.3 flag-vs-quota split — restated here 2026-07-28 so this
section stops contradicting it). Assignment runs on the commit path
([`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) §3.8).

## 4. Additional Context

### 4.1 Issue / Revoke from Transitions (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-issue-revoke-ent`

- On a successful resource-affecting transition whose outcome requires grants/withdrawals (activate, suspend, resume, cancel, Policy-gated plan/add-on/quantity changes) the gear issues/revokes entitlements to match the new posture and emits `EntitlementIssued`/`EntitlementRevoked` ([`../PRD.md`](../PRD.md) §6.9 AC 12).
- Issue/revoke is part of the **same commit** as the transition (slice 01) — posture never lags committed state.
- **Async edges bind the entitlement change to the status-edge commit (2026-07-19 review fix — F-05-3).** For an async OSS-leg edge (suspend/resume with a resource leg, cancel deprovision — slice 01 §3.6), "the same commit" is the **status-edge commit** (the one that fires on the OSS confirmation and bumps `version`), **not** the intent commit (`approved`, work order issued). So entitlements freeze/revoke exactly when the status actually changes, aligned with the OSS posture. If the transition is **abandoned** before its status edge (terminal `oss_unconfirmed`/deny — slice 01 §4.3 taxonomy), **no entitlement change was committed** — there is no window where an active subscription carries revoked entitlements, and none to compensate.
- **Cancel+new handover (2026-07-28 review fix — the slice-03 §4.3 "re-issue at one boundary" now has its owning mechanics here).** The handover spans **two aggregates and two commits**, so same-commit atomicity cannot hold across it; the ordering rule is **issue-before-revoke**: the successor's entitlements issue on its **activation status-edge commit** (OSS-confirmed), and the predecessor's revoke on its **cancel status-edge commit**, which slice 03 §4.3 sequences strictly after the successor's confirmation — so the customer never has an access **gap**, and the transient window is a bounded **double-grant**, not a dark period. The check surface treats a `supersedesSubscriptionId`-linked pair as **one continuity unit** for quota accounting during that window — mechanically a **read-time fold** (2026-07-28 review #19): `QuotaCounter` stays per-subscription (no pair-keyed table), and the surface resolves the pair via `supersedesSubscriptionId` and serves `max(predecessor, successor)` per quota, never the sum, for the handover window only. The successor's counters **seed from the predecessor's values at the handover boundary** — a cancel+new mid-period does **not** hand the customer a fresh cycle quota (the commercial continuity rule; the fold and the seed make an implementer's natural "just sum them" wrong twice). Exactly as the slice-03 overlap exemption treats the pair for cardinality. The successor's `GrantSetAssignment` resolves from the successor's own frozen grant set (no cross-aggregate copy); `supersedesSubscriptionId` on the successor is the join key the surface and consumers use to correlate `EntitlementIssued` (successor) with the later `EntitlementRevoked` (predecessor).
- **Freeze is a first-class state (2026-07-15 review fix):** `Entitlement.state ∈ {active, frozen, revoked}` — suspend **freezes** by default (restorable without re-materialisation; revoke only where product policy says so), resume **unfreezes**. **The same rule covers `SeatBinding` rows** (2026-07-28 review #7): suspend freezes active bindings, resume unfreezes them, and a **frozen binding still counts** toward the slice-03 quantity-decrease guard (the seat is committed, merely dormant — otherwise a suspension would silently license a quantity decrease below the bound count). Bind/release while `suspended` is rejected (`guard_violation` — posture is frozen). `QuotaCounter`s **persist across suspend/resume and `collectionPaused`** — a mid-cycle suspension does not reset the cycle's usage; counters reset only at the §4.4 cycle boundary.

### 4.2 Assignment from the Grant Set (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-assignment-ent`

- On activation, phase boundary, and committed plan/add-on change, entitlements (flags, usage quotas, resource quotas) are assigned from the plan's **published grant set** — including the **per-phase map** where phased (pricing **D-41**) — with immediate or end-of-cycle effective dates aligned to the transition's `changeMode` ([`../PRD.md`](../PRD.md) §6.9; SEAMS **SUB-P2**).
- The catalog authors the templates; this gear resolves + materialises the assignment per subscription — it authors no template.

### 4.3 Point-of-Use Check and Enforcement Split (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-check-split-ent`

- The gear exposes a real-time check read contract (flag decision, quota remaining, limit state) at **p95 < 100ms**, tenant-isolated + cache-friendly; **OSS enforces** (allow/block/degrade) — Subscriptions serves the decision state, never enforcement ([`../PRD.md`](../PRD.md) §6.9; SEAMS **SUB-E3**).
- Entitlement updates propagate to the check surface within the §7.1 baseline (< 5s).
- **Failure semantics (SUB-D-10, 2026-07-15 review fix):** the normal-mode cache and the outage mode follow **one staleness rule** — a decision may be served up to the **staleness budget** old (default 60s; the < 5s propagation is the normal-mode bound inside it); beyond the budget the check **fails closed**. This replaces the earlier self-contradictory "cache-first but never stale-allow" posture; a binary fail-closed read path would turn one projection outage into a platform-wide access block. Transitions (the Policy gate) remain strictly fail-closed — the budget applies to the **read** surface only. Decision cacheability/TTL toward OSS is part of the slice-09 contract and MUST fit inside the same budget.
- **Flag vs quota-remaining split (2026-07-15 review fix):** the staleness budget above governs **feature-flag** decisions (grant present/absent — a value that only changes on a committed transition, so 60s-stale is safe and fail-open-within-budget is correct). It does **not** license serving a stale **`quota remaining` / hard-limit** decision as *allow*: quota is a fast-moving counter, and a stale-allow past a `hard` threshold is exactly the "silent overrun" §4.4 forbids. The quota dimension of `CheckDecision` therefore carries its own tighter freshness contract — it is served from the authoritative `QuotaCounter` fold, and when that fold is stale beyond the **quota freshness bound — provisional default 10s, owner Product/OSS jointly with the staleness budget (PRD §15 row; ratify before Design lock, no bare placeholder ships)** — the quota decision **fails closed to `blocking`** (deny) rather than serving last-known-good. The flag and quota dimensions of one `CheckDecision` can thus degrade independently. The absolute usage→counter end-to-end lag stays the open NFR (§4.4); this fix pins only the *degradation posture*, which is a design call, not a Product knob.
- **Open (§15):** mid-request behaviour at the exhaustion instant (graceful degradation vs hard block) is an OSS/Design decision — pin the check-state ↔ enforcement contract with OSS at Design; the budget default is a Product/OSS knob.

### 4.4 Quota Soft/Hard Limits (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-quota-ent`

- Usage is tracked against quotas (aggregates fed by the rating pipeline); a **soft limit** crossing emits an auditable warning + MAY route overage per plan policy; a **hard limit** flips the check state to **blocking** — never a silent overrun ([`../PRD.md`](../PRD.md) §6.9). The events are `EntitlementQuotaWarning` / `EntitlementQuotaExhausted` / `EntitlementQuotaRestored` (SUB-D-09, slice 08 registry).
- **Quantity-scaled quotas — NOT AT LAUNCH (2026-07-29 cross-gear review, pricing D-66):** pricing publishes exactly two grant-set entry shapes, `featureFlag: bool` and `quotaKey: value` (pricing §17.6 contract table, design/06 `inst-gs-shape`), and carries **no** per-seat marker — seat-scaled allowance/entitlement is an explicitly named pricing **Future decision gate** (pricing PRD §1.4 "no per-seat scaling"; D-49(3)). Subscriptions MUST NOT infer one: a `quotaKey: 20` means 20 **total**, never 20/seat. Deferred until pricing authors a third entry shape plus the matching §17.6 contract row; at that point the materialised value becomes `grant × committed quantity @ t` (slice 03 `QuantityInterval`) with a committed `updateQuantity` re-materialising the affected quotas at its boundary per `changeMode`.
- **Cycle reset:** counters reset at the billing-anchor cycle boundary — triggered by the recurring period cut (slice 08), not by a separate clock, so quota cycles and billing periods can never drift apart. **Idempotent per period, not per cut (SUB-D-21, 2026-08-01 wave-3 review #9):** since SUB-D-19 a period has one cut **per component** plus possible targeted re-cuts — the reset fires on the period's **first** cut and is keyed `(subscriptionId, billing period)`, so a second component's cut or a §4.3 re-cut inside the same period never grants a fresh mid-period allowance. Two corollaries pinned 2026-07-28 (billing-pass review #20): **(a) during grace there is no reset** — grace suppresses the period cut, so counters deliberately do not refresh while no paid term exists (the customer keeps the old cycle's remainder, never a free fresh cycle); **(b) a backdated term extension** (late success in grace, §4.3b post-suspension) places the boundary in the past — the reset then applies **at the extension commit, forward-looking**: counters reset once, usage recorded between the backdated boundary and the commit stays where it accrued (no retroactive recount; the §17.1 reconciliation reads the same rule).
- **Propagation exposure:** the usage→check-state path (rating pipeline → `QuotaTracker` → surface) bounds how fast a hard limit takes effect; the end-to-end lag budget is an open NFR ([`../PRD.md`](../PRD.md) §15) — until set, the exposure is measured and reported by the §17.1 entitlement-sync reconciliation.
- Exhaustion + restore (new cycle, quota increase, plan change) emit auditable events.

### 4.5 Prepaid Grant Hook (deferred)

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-normative-prepaid-hook-ent`

- The prepaid-credit **definition** is pricing **D-43**; the **balance/drawdown** is Billing/Rating, **GA-gated** (SEAMS **SUB-B4**; [`../PRD.md`](../PRD.md) §2.2). Subscriptions supplies the subscription-side hook only (which subscription, which grant reference) — it neither defines nor draws down the wallet.
- Resolve the drawdown/tax placement jointly with pricing + Billing (pricing gap **G-4**) before prepaid GA; no launch dependency for the core lifecycle.

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §6.9 (`fr-entitlement-issue-revoke`, `fr-entitlement-assignment`, `fr-entitlement-check-contract`, `fr-entitlement-quota-limits`), §6.9 AC 12, §7.1 (check latency), §15 (mid-request open), §2.2 (prepaid).

**Traces to**: `cpt-cf-bss-subscriptions-fr-entitlement-assignment`, `cpt-cf-bss-subscriptions-fr-entitlement-issue-revoke`, `cpt-cf-bss-subscriptions-fr-entitlement-check-contract`, `cpt-cf-bss-subscriptions-fr-entitlement-quota-limits` *(single-owner FR claims — the P2 traceability convention adopted 2026-08-01, wave-3 review #24h; shared mechanics other slices cite stay narrative, each FR has exactly one owning slice)*
- **Seams**: **SUB-P2**, **SUB-E3**, **SUB-B4** — [`../SEAMS.md`](../SEAMS.md).
- **Slices**: [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (commit-time issue/revoke), [`02-composition-versioning.md`](./02-composition-versioning.md) (phase @ `t`), [`06-trials.md`](./06-trials.md) (per-phase re-issue), [`08-events-billing.md`](./08-events-billing.md) (entitlement events), [`09-consumer-contracts.md`](./09-consumer-contracts.md) (OSS check contract).
