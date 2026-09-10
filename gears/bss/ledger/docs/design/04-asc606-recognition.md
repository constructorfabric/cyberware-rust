Created:  2026-07-07 by Virtuozzo International GmbH
Updated:  2026-07-07 by Virtuozzo International GmbH

<!-- migration-note: converted from the legacy vhp-architecture design slice
     docs/bss/design/DESIGN-billing-ledger-balances-202606091200/04-DESIGN-billing-ledger-asc606-recognition-202606091400.md
     to the gears-sdlc design-slice layout (cpt-* sub-IDs, CDSL flows). The original is preserved unchanged in the
     vhp-architecture repository. All Foundation
     engine mechanics (PostingService, IdempotencyGate, BalanceProjector, MoneyModule, FiscalPeriodGuard, commit
     trigger, TieOutJob, outbox relay, total fixed lock order) are specified in ./01-repository-foundation.md and are referenced
     here, not restated. -->

# DESIGN — ASC 606 Revenue Recognition (Slice 4)

<!-- toc -->

- [1. Context](#1-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Naming Conventions and Design-Introduced Names](#15-naming-conventions-and-design-introduced-names)
  - [1.6 Scope and Constraints](#16-scope-and-constraints)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Trigger Recognition Run](#trigger-recognition-run)
  - [Apply Schedule Change](#apply-schedule-change)
  - [Read Recognition Schedules and Disaggregation](#read-recognition-schedules-and-disaggregation)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [ScheduleBuilder (Deferral and Timing Policy)](#schedulebuilder-deferral-and-timing-policy)
  - [Segment Release (S6 Posting)](#segment-release-s6-posting)
  - [PO Identification and SSP Allocation](#po-identification-and-ssp-allocation)
  - [Disaggregation Derivation and Mixed-Invoice Split](#disaggregation-derivation-and-mixed-invoice-split)
- [4. States (CDSL)](#4-states-cdsl)
  - [Recognition Schedule State Machine](#recognition-schedule-state-machine)
  - [Recognition Segment State Machine](#recognition-segment-state-machine)
  - [Recognition Run State Machine](#recognition-run-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Schedule Materialization](#schedule-materialization)
  - [Recognition Run and Segment Release](#recognition-run-and-segment-release)
  - [PO and SSP Gates](#po-and-ssp-gates)
  - [Revenue-Stream Disaggregation](#revenue-stream-disaggregation)
  - [Schedule Change Control](#schedule-change-control)
  - [Audit Linkage and Invariants](#audit-linkage-and-invariants)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Functional Considerations](#7-non-functional-considerations)
- [8. REST API Surface](#8-rest-api-surface)
  - [8.1 Endpoints](#81-endpoints)
  - [8.2 Queued Semantics (202)](#82-queued-semantics-202)
  - [8.3 Problem Responses (RFC 9457)](#83-problem-responses-rfc-9457)
- [9. Data Model (Slice-Owned Tables)](#9-data-model-slice-owned-tables)
  - [9.1 recognition_schedule](#91-recognition_schedule)
  - [9.2 recognition_segment](#92-recognition_segment)
  - [9.3 recognition_run](#93-recognition_run)
  - [9.4 Cross-Table Constraints and Enum Usage](#94-cross-table-constraints-and-enum-usage)
- [10. Events and Alarms](#10-events-and-alarms)
- [11. Decision Log and Open Items](#11-decision-log-and-open-items)
  - [11.1 Risks and Deferred Work](#111-risks-and-deferred-work)
  - [11.2 Needs Discussion (R1–R6, NFR)](#112-needs-discussion-r1r6-nfr)

<!-- /toc -->

## 1. Context

### 1.1 Overview

Turns **deferred revenue** (Contract liability posted at invoice by the Foundation S1 direct split) into **recognized Revenue** over time per ASC 606: documented recognition schedules, **atomic** idempotent recognition runs (`DR Contract liability / CR Revenue`), deferral/timing policy derivation, PO/allocation tagging + SSP allocation, revenue-stream disaggregation, and audit-grade linkage. ERP/GL export of recognized revenue is slice 7.

**Traces to**: `cpt-cf-bss-ledger-fr-recognition-schedule-controls`, `cpt-cf-bss-ledger-fr-recognition-audit-linkage`, `cpt-cf-bss-ledger-fr-asc606-po-identification`, `cpt-cf-bss-ledger-fr-revenue-stream-disaggregation`, `cpt-cf-bss-ledger-fr-invoice-post-direct-split`, `cpt-cf-bss-ledger-fr-policy-versioning-immutability`, `cpt-cf-bss-ledger-fr-negative-balance-invariants`, `cpt-cf-bss-ledger-fr-idempotency-per-flow`, `cpt-cf-bss-ledger-fr-idempotent-replay-contract`, `cpt-cf-bss-ledger-fr-out-of-order-event-handling`, `cpt-cf-bss-ledger-fr-money-rounding-scale`, `cpt-cf-bss-ledger-fr-accounting-periods-close`, `cpt-cf-bss-ledger-nfr-posting-performance`, `cpt-cf-bss-ledger-nfr-availability`

### 1.2 Purpose

**Slice boundary with the Foundation.** The Foundation **posts the Contract-liability credit line at invoice post** (S1 direct split) and owns the `CONTRACT_LIABILITY` / `REVENUE` account classes and the `revenue_stream` NOT-NULL line invariant. **This feature owns the RELEASE** (S6 recognition runs) and the **policy** that decides deferral, timing, PO/allocation tagging, SSP allocation, and disaggregation. It does **not** redefine the S1 posting or those account classes — it references them. *(Confirmed in the Foundation review: S1 posts the credit, S6 releases it.)*

Success criteria: no deferred balance ever exists without a schedule; each segment releases at most once per period; over-recognition is blocked at the per-schedule grain; every recognition entry carries full audit linkage; recognition is accrual-driven (independent of cash/collections) and gated only by obligation satisfaction.

**Requirements**: `cpt-cf-bss-ledger-fr-recognition-schedule-controls`, `cpt-cf-bss-ledger-fr-recognition-audit-linkage`, `cpt-cf-bss-ledger-fr-asc606-po-identification`, `cpt-cf-bss-ledger-fr-revenue-stream-disaggregation`, `cpt-cf-bss-ledger-nfr-posting-performance`

**Use cases**: `cpt-cf-bss-ledger-usecase-ledger-inquiry`, `cpt-cf-bss-ledger-usecase-exception-resolution`, `cpt-cf-bss-ledger-usecase-reconciliation-review`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-bss-ledger-actor-recognition-run` | Scheduled/event-triggered orchestration that releases due segments per period through the Foundation PostingService |
| `cpt-cf-bss-ledger-actor-catalog-contracts` | Contracts: deferral terms, recognition pattern, PO/allocation group, SSP override, VC estimate/method (authoritative). Catalog: SKU/Plan defaults, PO satisfaction pattern, SSP baseline, revenue-stream category. Consumed by **immutable version ref**, resolved locally |
| `cpt-cf-bss-ledger-actor-rating-subscriptions` | Subscriptions: obligation-satisfaction **state** that drives WHEN runs execute (eventually-consistent SoT) |
| `cpt-cf-bss-ledger-actor-finance-ops` | Resolves recognition exception-queue items (policy conflicts, SSP gaps, modification-treatment review); reads disaggregated revenue |
| `cpt-cf-bss-ledger-actor-finance-approver` | Approves controlled schedule changes (dual-control per policy, audit trail mandatory); owns R1/R2/R5/R6 value tables |
| `cpt-cf-bss-ledger-actor-revenue-assurance` | Receives recognition-double-credit / over-recognition alarms |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — § Posting rules S6, § ASC 606 compliance, § Recognition controls, § Out-of-order events, AC #4/#9/#17/#26, Example A
- **Design**: [01-repository-foundation.md](./01-repository-foundation.md) — Foundation engine (`PostingService` — one ACID txn per entry, READ COMMITTED; append-only journal + strict line-negation reversal; `IdempotencyGate` — 3-column `idempotency_dedup` PK, idempotent-replay; `MoneyModule` — banker's rounding + residual-cent rules; `BalanceProjector` — upsert + conditional no-negative; `FiscalPeriodGuard`; leaf-partition commit trigger; total fixed lock order; `TieOutJob`). Not restated here; RFC 2119 keywords are normative.
- **Dependencies**: Foundation / posting-engine-core (slice 1) upstream — posted Contract-liability credit (per stream) + invoice-item link; Contracts, Catalog, Subscriptions upstream (policy/state). Downstream: reconciliation-export (slice 7) — recognized-revenue export to ERP/GL + tie-out.
- Upstream module PRDs (contracts/catalog/subscriptions) are referenced from [PRD.md](../PRD.md) refs.

**Canonical slice numbering:** 1 posting-engine-core, 2 payments-allocation, 3 adjustments-notes-refunds, **4 asc606-recognition (this feature)**, 5 fx-multicurrency, 6 audit-immutability-observability, 7 reconciliation-export, 8 other. The `source_doc_type` / idempotency `flow` value `RECOGNITION` is **Foundation-declared**; this feature uses it.

### 1.5 Naming Conventions and Design-Introduced Names

Design-introduced names (this feature):

| Name | Meaning |
|------|---------|
| `recognition_schedule` | Documented release plan for **one single-revenue-stream** deferred balance: links to originating posted invoice item(s), PO/allocation group, currency, total deferred, recognized-to-date, policy/SSP/VC refs, status. |
| `recognition_segment` | One time- or milestone-slice of a schedule: period, amount, status (`PENDING`/`QUEUED`/`DONE`), recognized timestamp/run linkage. The **at-most-once unit** (one per `(schedule, period)`). |
| `recognition_run` | An orchestration wrapper that releases due segments for a period; **not** itself the dedup key. |

### 1.6 Scope and Constraints

**Non-goals / out of scope:**

- **S1 invoice-post posting & the `CONTRACT_LIABILITY`/`REVENUE` account classes** — Foundation (referenced, not restated).
- **Unbilled receivable / contract asset** — out of scope for the whole MVP (PRD § Out of scope; § Resolved decisions). Recognition without a resolvable posted-invoice-item link MUST block.
- **Gross-Revenue-then-reclassification** posting — not the default BSS pattern (PRD); the Foundation uses direct split only.
- **ERP/GL export, reconciliation** — slice 7. **Payments / refunds / FX** — slices 2/3/5. CreditApplication and refunds **never** touch Revenue or Contract liability (PRD).
- **Cancellation/replacement *decisions*** — owned upstream (Contracts/Subscriptions); the ledger consumes the decision and applies it prospectively / via compensating entries.
- **Engine mechanics** — Foundation (01-repository-foundation.md).

**In scope:**

- S6 recognition runs + schedules (§ Posting rules S6; AC #9)
- **release** of the Foundation deferred credit (AC #4 is the post; S6 is the release)
- Identify-POs + immaterial-one-shot exemption
- SSP (multi-PO) vs documented-pricing (single-PO) + variable consideration
- deferral-policy + recognition-timing derivability/precedence
- recognition-vs-cash separation (accrual)
- schedule integrity (no double recognition)
- revenue-stream **disaggregation** derivation + mixed-invoice split (the line-tag invariant AC #26 is Foundation; derivation+split is this feature)
- audit minimum linkage + invoice-item-link-or-block
- **obligation-satisfaction (not collections) gating** of runs (AC #21 interaction)
- Contract>Catalog precedence
- recognition idempotency/dedup/rerun/residual-cent/ordering/schedule-change control
- recognition reversal/clawback shape
- per-schedule over-recognition guard (AC #17 interaction)
- recognition-double-credit alarm
- recognition run window NFR

**Consumed:** posted Contract-liability balance (per revenue-stream line) + originating invoice-item link (Foundation); deferral/timing/SSP/VC **policy** + PO/allocation group (Contracts authoritative, Catalog default; consumed by **immutable version ref**); subscription obligation-satisfaction **state** (Subscriptions). **(Ingestion model — README):** policy/SSP refs resolve **locally** (immutable version refs) and recognition runs are **call/schedule-driven** — no inbound bus on the post path (C3); "consumed" names the local fact, not a subscription. **Produced:** recognition journal entries (`DR CONTRACT_LIABILITY / CR REVENUE`, same stream both legs), schedule state, double-credit/over-recognition alarms.

**Constraints and assumptions** (inherits Foundation C1–C4 + A1–A6). Feature-specific (defaults; open items → §11.2):

| # | Topic | Assumption (default) | Source |
|---|-------|----------------------|--------|
| R1 | Deferral-policy precedence | **Contract → Catalog SKU/Plan → PO type → billing model**; Contract overrides Catalog for the same dimension. **Skeleton ratified 2026-06-10** (same-dimension conflict → Contract wins; unresolvable ambiguity → block + exception queue); value tables are Finance-owned data. | PRD |
| R2 | Recognition-timing precedence | **Contract → Catalog + PO type → subscription state** (state drives *when*, not the *pattern* unless Contract ties it). **Skeleton ratified 2026-06-10**; value tables are Finance-owned data. | PRD |
| R3 | SSP source of truth | **Ratified 2026-06-10:** Finance approves SSP policy; **Contract override authoritative, Catalog baseline fallback**; versioned immutable snapshots **pinned at contract inception, referenced per post** (— reused across the contract's invoices, never re-snapshotted per invoice). The blanket multi-PO block is lifted; `SSP_SNAPSHOT_REQUIRED` remains the per-post **presence** guard. | PRD |
| R4 | Immaterial-one-shot exemption | Point-in-time, no deferral/schedule, ≤ **1% of invoice total or 100 USD-equiv (lower)**, Catalog-SKU-flagged; tenant-configurable — **ratified 2026-06-10**. | PRD |
| R5 | Variable-consideration vs multi-PO tie-break | **Ratified 2026-06-10:** multi-PO + VC → SSP path mandatory; single-PO VC → documented estimate + method only. | PRD |
| R6 | Disaggregation attributes | Streams **usage / recurring / one-time** as a distinct `account_id` per stream — **CoA pattern ratified 2026-06-10** (`tenant_account.revenue_stream` + widened CoA UNIQUE — Foundation); stream **names** remain Finance-confirmable. | PRD |

`ScheduleBuilder` materializes a `recognition_schedule` **in the same transaction as the Foundation Contract-liability credit** (no outbox path), so a deferred balance never exists without a schedule. `RecognitionRunner` executes per period through the Foundation `PostingService`.

## 2. Actor Flows (CDSL)

### Trigger Recognition Run

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-recognition-run`

**Actor**: `cpt-cf-bss-ledger-actor-recognition-run`

A scheduled/event run releases due segments for a period. Each released **segment** produces **one balanced entry**, posted through the Foundation `PostingService` via `cpt-cf-bss-ledger-algo-recognition-segment-release`:

| Line | Side | Account class |
|------|------|---------------|
| Recognize | DR | `CONTRACT_LIABILITY` (segment's stream) |
| Revenue | CR | `REVENUE` (same `revenue_stream`) |

**At-most-once per segment per period.** Idempotent per `(tenant, RECOGNITION, schedule_id:segment_no)` (Foundation `IdempotencyGate`); the `recognition_segment` `status=DONE`/`run_id` + the `UNIQUE (schedule, period_id)` key prevent a second credit. The **run** is an orchestration wrapper (dedup `(tenant, period_id, runId)` at the orchestration layer, plus a per-`(tenant, period_id)` advisory lock / single-active-run guard so overlapping runs serialize); each segment is independently at-most-once via the Foundation gate — overlapping **different** runIds cannot double-credit a segment.

**Run gating.** Whether to run keys on **obligation-satisfaction** state, **not** collections/dunning: a collections-suspended payer's schedule keeps recognizing (revenue is earned regardless of collection); only an upstream **cancellation** (obligation ceased) stops/changes the schedule (PRD). **(/ S4-minor) Freshness contract.** The consumed obligation-satisfaction state is **eventually-consistent** (Subscriptions is the SoT); run-gating reads the latest known state at run time. **Fail-safe default:** an **unknown or stale** state is treated as **"not satisfied"** — recognition is delayed (and surfaced at the slice 7 close gate via undone-due-segment blocking), never released early; a stale "satisfied" can only ever release up to `total_deferred_minor` (the per-schedule CHECK caps it). Staleness beyond a configured watermark raises an operational alarm rather than silently delaying revenue.

**Ordering (feature-owned mechanism).** Before releasing the segment for period N, the runner asserts all lower-`period_id` segments of the **same schedule** are `DONE`; if not, the segment is marked **`QUEUED`** and the request returns **202** with body status token `recognition-period-queued` (kebab-case; no SCREAMING_SNAKE code on a 202 — deferral convention uniform across slices 2/3/4). A later run picks up `QUEUED` segments once the predecessor commits. (Queue-vs-commit behavior per PRD; the mechanism is defined here, not inherited.)

**Success Scenarios**:
- Due segments release; per-stream Contract liability drains; segments stamped `DONE` atomically with the posting
- Run replay returns the prior run reference (Foundation AC #19)

**Error Scenarios**:
- Out-of-order period → segment `QUEUED`, 202 `recognition-period-queued`
- Over-release attempt → 409 `OVER_RECOGNITION` (per-schedule CHECK)
- Unknown/stale obligation-satisfaction state → recognition delayed (fail-safe), staleness watermark alarm

**Steps**:
1. [ ] - `p1` - Scheduler or client calls API: POST /v1/ledger/recognition-runs (body: period_id, runId) - `inst-run-api`
2. [ ] - `p1` - Dedup the run trigger on `(tenant, period_id, runId)` at the orchestration layer; **IF** replay **RETURN** prior run reference - `inst-run-dedup`
3. [ ] - `p1` - Acquire the per-`(tenant_id, period_id)` single-active-run advisory lock so overlapping runs serialize - `inst-run-lock`
4. [ ] - `p1` - DB: Select due `recognition_segment` rows (status `PENDING`/`QUEUED`, period ≤ run period) via partition-pruned schedule scan - `inst-run-select`
5. [ ] - `p1` - **FOR EACH** due segment - `inst-run-foreach`
   1. [ ] - `p1` - **IF** obligation-satisfaction state for the schedule is unknown/stale/not-satisfied: skip (fail-safe "not satisfied"; alarm beyond the staleness watermark) - `inst-run-gate`
   2. [ ] - `p1` - **IF** any lower-`period_id` segment of the same schedule is not `DONE`: mark segment `QUEUED`, **RETURN** 202 `recognition-period-queued` + correlation handle - `inst-run-order`
   3. [ ] - `p1` - **ELSE** release via `cpt-cf-bss-ledger-algo-recognition-segment-release` (one balanced entry per segment) - `inst-run-release`
6. [ ] - `p1` - Emit `billing.ledger.revenue.recognized` per released segment (§10) - `inst-run-event`
7. [ ] - `p1` - **RETURN** run reference (status RUNNING → DONE/FAILED) - `inst-run-return`

### Apply Schedule Change

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-recognition-schedule-change`

**Actor**: `cpt-cf-bss-ledger-actor-catalog-contracts`

- **Controlled changes.** Rate/period/catch-up/cancellation/reallocation changes require approval where policy demands, an audit trail, and **either** a new schedule **version** (effective-dated) **or** **compensating** journal entries — **never** a silent rewrite of already-released amounts. **A new schedule version is minted with a new `schedule_id`**: the `version` column labels lineage, but the **`schedule_id` is what makes the release key `schedule_id:segment_no` version-distinct** — so a re-versioned schedule's segments can never collide with the prior version's `DONE` segments. A legitimate post-re-version catch-up release is therefore allowed, and a double release under a reused key is structurally impossible. An **S3 credit note** (slice 3) that reduces unreleased deferred revenue is an authorized **prospective-reduction** trigger: it decrements `total_deferred_minor` over the not-yet-released remainder under the shared lock order, never touching already-released segments.
- **Cancel/replace decision is upstream** (Contracts/Subscriptions). On a cancel/replace event the ledger marks the schedule `CANCELLED`/`REPLACED` and applies the new schedule **prospectively** or posts compensating entries as instructed (PRD). **Treatment marking — v1 control.** The cancel/replace (`/changes`) event MUST carry the intended **`treatment`** (`prospective` | `separate_contract` | `catch_up`). `prospective` / `separate_contract` apply directly (the usual SaaS series-of-distinct-services case, ASC 606-10-25-13(a)); a **`catch_up`** modification (ASC 606-10-25-13(b) — remaining goods/services not distinct) **and** any **unmarked / unknown** treatment **route to the exception queue** (`MODIFICATION_TREATMENT_REVIEW`) until the VC & contract-modifications successor PRD ships — never silently applied prospectively (PRD § Periodic recognition controls).

**Success Scenarios**:
- Prospective / separate-contract change applies; old schedule marked `REPLACED`/`CANCELLED`; new version minted with a new `schedule_id`

**Error Scenarios**:
- `catch_up` or unmarked treatment → exception queue `MODIFICATION_TREATMENT_REVIEW`, never silently applied
- Change replay per change id returns the prior reference

**Steps**:
1. [ ] - `p1` - Upstream calls API: POST /v1/ledger/recognition-schedules/{scheduleId}/changes (body: change id, treatment, instructed effect) - `inst-chg-api`
2. [ ] - `p1` - Claim idempotency per change id; **IF** replay **RETURN** prior reference - `inst-chg-idem`
3. [ ] - `p1` - Enforce approval / dual-control where policy demands; record the audit trail - `inst-chg-approval`
4. [ ] - `p1` - **IF** treatment == catch_up **OR** treatment unmarked/unknown: **RETURN** route to exception queue (`MODIFICATION_TREATMENT_REVIEW`) - `inst-chg-treatment`
5. [ ] - `p1` - **IF** cancel: mark schedule `CANCELLED`; **IF** replace/re-version: mark old schedule `REPLACED` and mint the new version with a **new `schedule_id`** (lineage via `version`), recomputing residual only over the not-yet-released remainder - `inst-chg-version`
6. [ ] - `p1` - **IF** instructed: post compensating journal entries instead of / alongside the new version — never rewrite already-released amounts - `inst-chg-compensate`
7. [ ] - `p1` - Emit `billing.ledger.schedule.changed` (§10) - `inst-chg-event`
8. [ ] - `p1` - **RETURN** 200 change reference - `inst-chg-return`

### Read Recognition Schedules and Disaggregation

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-flow-recognition-inquiry`

**Actor**: `cpt-cf-bss-ledger-actor-finance-ops`

**Success Scenarios**:
- Finance reads a schedule with segments + recognized-to-date; reads recognized revenue by stream/period

**Error Scenarios**:
- Unknown scheduleId → 404; cross-tenant read blocked by RLS

**Steps**:
1. [ ] - `p1` - API: GET /v1/ledger/recognition-schedules/{scheduleId} — DB: read `recognition_schedule` + `recognition_segment` rows - `inst-rinq-schedule`
2. [ ] - `p1` - API: GET /v1/ledger/revenue/disaggregation — cache/query of recognized revenue by stream/period (per-stream `account_balance` grains) - `inst-rinq-disagg`
3. [ ] - `p1` - **RETURN** 200 (schedule + segments + recognized-to-date / disaggregated revenue) - `inst-rinq-return`

## 3. Processes / Business Logic (CDSL)

### ScheduleBuilder (Deferral and Timing Policy)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-schedule-builder`

**Input**: a posting that creates deferred revenue (S1 invoice path or S4 correction path — debit notes, slice 3), item attributes, local materialized policy/SSP/VC snapshots
**Output**: a `recognition_schedule` + segments materialized in the **same transaction** as the Contract-liability credit, or a failed post (no orphan deferral)

`ScheduleBuilder` runs **in the same transaction as the posting** — for both the S1 invoice path and the S4 correction path (debit notes, slice 3 D4) — so no deferred balance is ever left without a schedule; if schedule derivation cannot complete, **the post fails** (no orphan deferral). The outbox alternative is removed; `UNSCHEDULED_DEFERRAL` remains a defense-in-depth close predicate (slice 7) and additive `exception_queue` type emitted by this feature; unresolvable R1/R2/R5 ambiguity opens an `exception_queue` row of type `RECOGNITION_POLICY_CONFLICT` (slice 7).

**Blast radius + recovery (normative).** Because derivation is in-txn, a recognition-config gap fails the **invoice** post — this is stated and accepted (the cost of the no-orphan-deferral invariant). But every such block MUST have an **operational recovery path**, not a silent 422: `SSP_SNAPSHOT_REQUIRED` (and any other recognition-derived block) **also opens an `exception_queue` row + Finance alert** (parity with `RECOGNITION_POLICY_CONFLICT`) with a clear **retry once the config/snapshot lands**. Derivation is **local** → a deterministic config gap, never a remote-availability failure.

**Idempotency (🔄 — decoupled from status):** the builder claims a Foundation `idempotency_dedup (tenant, flow=SCHEDULE_BUILD, business_id=source_invoice_id:source_invoice_item_ref:revenue_stream)` row, so a replayed or raced build returns the prior reference, never a duplicate — **independent of the schedule's lifecycle `status`** (operation-key-vs-row-key split, per the slice 2 pattern). The table's partial `UNIQUE … WHERE status='ACTIVE'` remains as the at-most-one-live guard. A fully-recognized schedule transitions `ACTIVE → COMPLETED` (terminal); the dedup row persists, so the duplicate key holds permanently even after the `COMPLETED` schedule is archived/partitioned. `REPLACED` versioning keeps history.

**Bound (default — §11.2; 🔄):** the **120-segment** default is a guardrail, not a hard wall — derivation beyond it **degrades** (coarser segmentation / chunked multi-record schedule, full amount over the full horizon) rather than failing a valid long-horizon / daily contract; 120 to be confirmed against real catalog terms and the degrade strategy confirmed with Finance.

**Deferral** is decided by precedence R1, **timing** by R2; subscription obligation-satisfaction state (R2) drives *when* runs execute, not the pattern. The derivation reads **versioned, immutable** policy/SSP/VC refs and stamps them on the schedule (historical immutability — a later policy change never rewrites an existing schedule). These refs resolve from the **local** database (materialized policy/SSP snapshots plus the post request's item attributes) — the in-transaction derivation makes **no network call into Contracts or Catalog**, so Foundation C3 (no external blocking dependency on the post path) holds and a Contracts outage cannot take down posting; a stale or missing local snapshot fails the post deterministically rather than blocking on a remote service. R1/R2/R5 precedence skeleton + conflict rules **ratified 2026-06-10** (same-dimension → Contract wins; unresolvable ambiguity → block + exception queue); value tables are Finance-owned data. VC estimate/method evidence is referenced read-only via `vc_estimate_ref`/`vc_method_ref` (or the Contract artifact via `policy_ref`).

**Steps**:
1. [ ] - `p1` - Claim `idempotency_dedup (tenant, SCHEDULE_BUILD, source_invoice_id:source_invoice_item_ref:revenue_stream)`; **IF** replay/race **RETURN** prior reference (independent of schedule `status`) - `inst-sb-idem`
2. [ ] - `p1` - Resolve deferral by R1 precedence (Contract → Catalog SKU/Plan → PO type → billing model) and timing by R2 (Contract → Catalog + PO type → subscription state) from **local** immutable snapshots — no network call on the post path - `inst-sb-policy`
3. [ ] - `p1` - **IF** same-dimension conflict: Contract wins; **IF** unresolvable ambiguity: fail the post + open `exception_queue` row `RECOGNITION_POLICY_CONFLICT` - `inst-sb-conflict`
4. [ ] - `p1` - Algorithm: apply PO/SSP gates via `cpt-cf-bss-ledger-algo-recognition-po-ssp-allocation` (may fail the post with 422 + exception-queue row + Finance alert) - `inst-sb-possp`
5. [ ] - `p1` - Derive segments (time- or milestone-sliced); **IF** derivation exceeds 120 segments: **degrade** (coarser segmentation / chunked multi-record schedule over the full horizon) rather than failing a valid long-horizon contract - `inst-sb-segments`
6. [ ] - `p1` - DB: Insert `recognition_schedule` (+segments) in the **same txn** as the Contract-liability credit, stamping immutable `policy_ref` / `ssp_snapshot_ref` / `vc_estimate_ref` / `vc_method_ref` - `inst-sb-insert`
7. [ ] - `p1` - **IF** derivation cannot complete: fail the post (no orphan deferral); `UNSCHEDULED_DEFERRAL` remains the defense-in-depth close predicate - `inst-sb-fail`
8. [ ] - `p1` - **RETURN** schedule reference - `inst-sb-return`

### Segment Release (S6 Posting)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-recognition-segment-release`

**Input**: a due `recognition_segment` (predecessors `DONE`, gating satisfied), run context
**Output**: one balanced `DR CONTRACT_LIABILITY / CR REVENUE` entry + segment stamp + counter increment, atomic

**Atomicity (critical).** Within the **same** Foundation `PostingService` transaction that posts the DR/CR, the runner also stamps the segment (`status=DONE`, `recognized_at`, `run_id`) and increments `recognition_schedule.recognized_minor` by an **in-place delta** — `SERIALIZABLE`/SSI-serialized, NOT a `FOR UPDATE` row lock — (`SET recognized_minor = recognized_minor + amount`, `CHECK` evaluated post-delta). Journal + segment stamp + counter commit or roll back together. `recognition_schedule` and `recognition_segment` are added to the **total fixed write/UPSERT order** (`table_rank` just below the balance caches, ordered by `(tenant_id, schedule_id)` then `segment_no`), so concurrent posts serialize under SSI — Postgres detects the write-write conflict and retries; there is no `FOR UPDATE` row locking. **Merged ordering:** acquire in ascending `table_rank` — the `CONTRACT_LIABILITY` + `REVENUE` `account_balance` rows in the Foundation's existing `(table_rank, tenant_id, account_id, currency, …)` order **first**, then the recognition tables by `(tenant_id, schedule_id, segment_no)`; one global order across all recognition posts.

**Authoritative over-recognition guard.** The **per-schedule** `CHECK (recognized_minor ≤ total_deferred_minor)` — the only grain that maps 1:1 to an obligation — is the in-transaction, lock-ordered guard; failure → `OVER_RECOGNITION` (409). The aggregate `CONTRACT_LIABILITY` no-negative `CHECK` (Foundation) is **defense-in-depth** only (it would not catch over-release of one schedule while a sibling keeps the account aggregate positive). Cumulative releases ≤ posted Contract liability is reconciled by the tie-out.

**Reversal/clawback.** A recognition reversal is a new `DR REVENUE / CR CONTRACT_LIABILITY` entry (its own key `schedule_id:segment_no:reversal`, flow `RECOGNITION`) that decrements `recognized_minor` in the same transaction; Revenue decreasing here is a legitimate reversal, not a sign violation. A reversed segment stays `status=DONE` (its release happened and was compensated); re-recognizing that period requires a **new schedule version**, never a silent re-release under the same key.

**Accrual, not cash.** Recognition is independent of any payment/settlement (slice 2). **Residual cent → last segment** of the **schedule version** that owns it (a re-version recomputes residual only over the not-yet-released remainder; already-released segments are never recomputed) (Foundation `MoneyModule`, PRD).

**Period assignment.** A release entry posts with the **segment's `period_id`** while that fiscal period is OPEN. The period-N close gate (slice 7) blocks while any segment due ≤ N is not `DONE`. A segment that nonetheless misses close posts into the **current open period**, with the original target period recorded as audit linkage — never into a closed period (Foundation `FiscalPeriodGuard`).

**Steps**:
1. [ ] - `p1` - Claim idempotency: `idempotency_dedup (tenant, RECOGNITION, schedule_id:segment_no)`; **IF** replay **RETURN** prior reference - `inst-rel-idem`
2. [ ] - `p1` - Acquire writes in ascending `table_rank`: `CONTRACT_LIABILITY` + `REVENUE` `account_balance` rows first (Foundation order), then `recognition_schedule`/`recognition_segment` by `(tenant_id, schedule_id, segment_no)` - `inst-rel-order`
3. [ ] - `p1` - Post the balanced entry DR `CONTRACT_LIABILITY` / CR `REVENUE` — DR and CR carry the **same** `revenue_stream` - `inst-rel-post`
4. [ ] - `p1` - Assert (re-asserted at run time) `source_invoice_item_ref` resolves to a posted `CONTRACT_LIABILITY` `journal_line`; **IF** not **RETURN** 422 `RECOGNITION_WITHOUT_INVOICE_LINK` - `inst-rel-link`
5. [ ] - `p1` - **IF** the segment's `period_id` fiscal period is OPEN: post with that `period_id`; **ELSE** post into the current open period recording the original target period as audit linkage (never a closed period) - `inst-rel-period`
6. [ ] - `p1` - DB: Same txn — stamp segment `status=DONE`, `recognized_at`, `run_id`; `SET recognized_minor = recognized_minor + amount` (SSI-serialized in-place delta, CHECK post-delta) - `inst-rel-stamp`
7. [ ] - `p1` - **IF** `CHECK (recognized_minor ≤ total_deferred_minor)` fails: **RETURN** 409 `OVER_RECOGNITION` (whole txn rolls back) - `inst-rel-overrec`
8. [ ] - `p1` - Attach the residual cent to the **last segment** of the owning schedule version - `inst-rel-residual`
9. [ ] - `p1` - **RETURN** posting reference (journal + stamp + counter atomic) - `inst-rel-return`

### PO Identification and SSP Allocation

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-recognition-po-ssp-allocation`

**Input**: posted invoice item attributes, Contract/Catalog PO + SSP + VC refs (local immutable snapshots)
**Output**: PO/allocation-group tagging + SSP-allocated transaction price, or a blocking gate (422 + exception queue)

- **Identify POs.** A posted invoice item MUST carry a PO/allocation-group id when (1) it has deferral/a schedule, (2) Contract/Catalog marks it multi-PO/ASC-tracked/SSP-required, or (3) variable consideration applies; otherwise a Catalog **default** allocation group. The only exemption is the **immaterial one-shot** (R4). Outside it, missing PO **blocks post** — the invoice-posting handler (Foundation) enforces the *presence* gate; **this feature owns the trigger conditions + exemption**. The `MISSING_PO_ALLOCATION_GROUP` rule is **registered by this feature as a Foundation post-hook** that the post pipeline invokes (same posture as `flow=RECOGNITION`); the handler surfaces the code on its endpoint. **Block scope.** The Catalog **default** allocation group is the **primary mitigation**: an ordinary point-in-time line is **auto-tagged** and **never blocks** the invoice; `MISSING_PO_ALLOCATION_GROUP` fires **only** on a genuinely **ambiguous** deferred / multi-PO / VC line whose PO/allocation group cannot be resolved **and** cannot be defaulted — audit hygiene, not a tag-everything-or-stop gate on routine billing. **(/ S4-minor) Ownership boundary (explicit).** The invoice-posting handler (Foundation) owns **only** the *presence* gate on its post endpoint; **this feature owns the trigger conditions + R4 exemption**, shipped as a **Foundation post-hook** the post pipeline invokes (same posture as `flow=RECOGNITION`). A change to the trigger is a **feature-local** change to its own hook — **no mutation of another slice's endpoint** (the Repository-foundation post-hook mechanism; the cross-slice pattern is dissolved).
- **Transaction price.** **SSP snapshots** required for **multi-PO** allocation; **documented pricing adjustments** suffice for a **single-PO** line backed by immutable evidence. **Variable consideration** always carries a documented estimate + method (immutable refs). **VC posting boundary.** For the MVP all VC is usage billed in arrears on actuals (invoice = earned), so **no VC estimate / true-up posting is in scope**; VC accrual + period re-estimation + modification accounting (catch-up vs prospective) is a **named successor PRD** (Revenue — VC & contract modifications) owned with Contracts/Subscriptions — never a silent Design default.
- **SSP gate (R3 — ratified 2026-06-10).** The SSP source/precedence is committed (Catalog baseline → Contract override → Finance-approved versioned snapshots), so the blanket multi-PO block is **lifted**. `SSP_SNAPSHOT_REQUIRED` (422) remains as the **per-post guard**: a multi-PO schedule still cannot materialize when the required SSP snapshot ref is missing or unresolvable for that posting. In addition to the 422, a missing/unresolvable SSP snapshot **opens an `exception_queue` row + Finance alert** with a retry-once-loaded path — a config gap is never a silent dead-end. **Inception pinning.** The SSP snapshot **value** is pinned at **contract inception** (the first post for the contract) and the **same** versioned ref is reused for every subsequent invoice of that contract — never re-snapshotted per invoice (ASC 606 allocates the transaction price at inception and forbids re-allocation on later SSP changes). The per-post guard only checks the ref is **present and resolvable**; it does **not** re-pick a fresh SSP. For a single-invoice / single-PO line, inception = that post (so per-post and per-inception coincide). **Inception ties to the SSP *requirement*, not the first ledger post.** When a contract's first post is a **point-in-time single-PO line that needs no SSP**, that post pins **no** SSP value; SSP is pinned at the **first post that triggers an SSP requirement** (the first multi-PO / allocation line). A later multi-PO line lacking a resolvable SSP ref hits `SSP_SNAPSHOT_REQUIRED` (422 + exception queue, above) — it never mis-allocates silently on a routine bill.

**Steps**:
1. [ ] - `p1` - **IF** line is immaterial one-shot (R4: point-in-time, ≤ 1% invoice total or 100 USD-equiv (lower), SKU-flagged; tenant-configurable): exempt — no deferral/schedule/PO requirement - `inst-po-exempt`
2. [ ] - `p1` - **IF** line has deferral/schedule, is marked multi-PO/ASC-tracked/SSP-required, or carries VC: require a PO/allocation-group id; **ELSE** auto-tag the Catalog default allocation group (never blocks) - `inst-po-require`
3. [ ] - `p1` - **IF** PO/allocation group cannot be resolved and cannot be defaulted (genuinely ambiguous deferred/multi-PO/VC line): **RETURN** 422 `MISSING_PO_ALLOCATION_GROUP` via the Foundation post-hook - `inst-po-block`
4. [ ] - `p1` - **IF** multi-PO: require the contract-inception-pinned SSP snapshot ref (present + resolvable; pinned at the first SSP-requiring post, reused for all subsequent invoices, never re-picked); **IF** missing/unresolvable **RETURN** 422 `SSP_SNAPSHOT_REQUIRED` + `exception_queue` row + Finance alert (retry once the snapshot lands) - `inst-po-ssp`
5. [ ] - `p1` - **IF** single-PO: accept documented pricing adjustments backed by immutable evidence - `inst-po-single`
6. [ ] - `p1` - **IF** VC present: require documented estimate + method refs (`vc_estimate_ref`/`vc_method_ref`); multi-PO + VC → SSP path mandatory (R5); no VC true-up posting in MVP (successor PRD) - `inst-po-vc`
7. [ ] - `p1` - **RETURN** tagged + allocated line attributes for schedule materialization - `inst-po-return`

### Disaggregation Derivation and Mixed-Invoice Split

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-recognition-disaggregation-split`

**Input**: revenue-affecting lines of a posting (possibly a multi-stream bundle)
**Output**: per-stream Contract-liability lines + one schedule per stream; recognition entries stream-matched

Every revenue-affecting line carries a **mandatory revenue-stream classification** (usage / recurring / one-time) — the line-tag **invariant** is enforced in the Foundation; **this feature owns the derivation + the mixed-invoice split**. For a multi-stream bundle, the **Foundation Contract-liability credit is split into one deferred line per stream**, so the `CONTRACT_LIABILITY` balance is tracked per stream; this feature creates **one schedule per stream**, and a recognition segment's `revenue_stream` **MUST equal** the stream of the Contract-liability line it draws down (DR and CR carry the **same** stream). Per-stream Contract liability drains to zero (not just the aggregate). Streams map to distinct natural accounts / sub-accounts / reporting dimensions, not free text. **R6 — ratified 2026-06-10:** streams `usage / recurring / one-time` resolve to a **distinct `account_id` per stream** for both `REVENUE` and `CONTRACT_LIABILITY` (one `tenant_account` row per (class, stream, currency)). Consequences: per-stream balances come free via `account_balance`; the per-stream `CONTRACT_LIABILITY` **no-negative `CHECK` on `account_balance` is now the authoritative per-stream drain guard** (the per-schedule `recognized_minor ≤ total_deferred_minor` `CHECK` remains the per-obligation guard); the per-tenant credit-side hot row splits three ways. Stream **names** remain Finance-confirmable without affecting the mechanism.

**Steps**:
1. [ ] - `p1` - Derive the revenue-stream classification (usage / recurring / one-time) for each revenue-affecting line - `inst-dis-derive`
2. [ ] - `p1` - **IF** multi-stream bundle: split the Contract-liability credit into one deferred line per stream - `inst-dis-split`
3. [ ] - `p1` - Create **one `recognition_schedule` per stream** (a schedule is single-revenue-stream) - `inst-dis-schedule`
4. [ ] - `p1` - Resolve each stream to its distinct `account_id` (`tenant_account` row per (class, stream, currency)) for both `REVENUE` and `CONTRACT_LIABILITY` - `inst-dis-account`
5. [ ] - `p1` - Enforce: a recognition segment's `revenue_stream` equals the stream of the Contract-liability line it draws down; per-stream Contract liability drains to zero - `inst-dis-match`
6. [ ] - `p1` - **RETURN** per-stream lines + schedules (per-stream no-negative CHECK = authoritative drain guard) - `inst-dis-return`

## 4. States (CDSL)

### Recognition Schedule State Machine

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-state-recognition-schedule`

**States**: ACTIVE, COMPLETED, REPLACED, CANCELLED
**Initial State**: ACTIVE (at-most-one-live per `(tenant_id, source_invoice_id, source_invoice_item_ref, revenue_stream)` via partial UNIQUE)

**Transitions**:
1. [ ] - `p1` - **FROM** ACTIVE **TO** COMPLETED **WHEN** fully recognized (`recognized_minor == total_deferred_minor`, all segments `DONE`) — terminal; schedule + segments become archivable/partitionable while the `SCHEDULE_BUILD` dedup row persists (build idempotency decoupled from status, no duplicate-build hole) - `inst-st-rs-completed`
2. [ ] - `p1` - **FROM** ACTIVE **TO** REPLACED **WHEN** a controlled change mints a new version — new `schedule_id`, lineage via `version`; release keys can never collide with the prior version's `DONE` segments - `inst-st-rs-replaced`
3. [ ] - `p1` - **FROM** ACTIVE **TO** CANCELLED **WHEN** an upstream cancel decision arrives (obligation ceased); prospective effect / compensating entries as instructed - `inst-st-rs-cancelled`

### Recognition Segment State Machine

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-state-recognition-segment`

**States**: PENDING, QUEUED, DONE
**Initial State**: PENDING (`segment_no` immutable, 1:1 with `period_id`)

**Transitions**:
1. [ ] - `p1` - **FROM** PENDING **TO** DONE **WHEN** the segment releases (atomic with the DR/CR posting; `recognized_at` + `run_id` stamped) - `inst-st-seg-done`
2. [ ] - `p1` - **FROM** PENDING **TO** QUEUED **WHEN** the run finds a lower-`period_id` segment of the same schedule not `DONE` (202 `recognition-period-queued`) - `inst-st-seg-queued`
3. [ ] - `p1` - **FROM** QUEUED **TO** DONE **WHEN** a later run releases it after the predecessor commits - `inst-st-seg-resume`
4. [ ] - `p1` - A reversed segment **stays DONE** (its release happened and was compensated); re-recognizing that period requires a new schedule version, never a re-release under the same key - `inst-st-seg-reversal`

### Recognition Run State Machine

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-state-recognition-run`

**States**: RUNNING, DONE, FAILED
**Initial State**: RUNNING (orchestration wrapper only — never the dedup key; segments are independently at-most-once)

**Transitions**:
1. [ ] - `p1` - **FROM** RUNNING **TO** DONE **WHEN** all due segments for the period are released or queued - `inst-st-run-done`
2. [ ] - `p1` - **FROM** RUNNING **TO** FAILED **WHEN** the run aborts; a re-trigger with a new runId re-drives safely (per-segment idempotency prevents double credit) - `inst-st-run-failed`

## 5. Definitions of Done

### Schedule Materialization

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-asc606-recognition-schedule-build`

The system **MUST** materialize a `recognition_schedule` in the same transaction as the Contract-liability credit (S1 invoice path and S4 correction path), fail the post when derivation cannot complete (no orphan deferral), open exception-queue rows + Finance alerts for every recognition-derived block, claim `SCHEDULE_BUILD` idempotency decoupled from schedule status, stamp immutable policy/SSP/VC refs resolved locally (no network call on the post path), and degrade rather than fail beyond the 120-segment guardrail.

**Implements**:
- `cpt-cf-bss-ledger-algo-schedule-builder`
- `cpt-cf-bss-ledger-state-recognition-schedule`

**Touches**:
- DB: `recognition_schedule`, `recognition_segment`, `idempotency_dedup` (flow `SCHEDULE_BUILD`), `exception_queue`
- Entities: `RecognitionSchedule`, `RecognitionSegment`

### Recognition Run and Segment Release

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-asc606-recognition-run-release`

The system **MUST** release due segments per period as one balanced entry each (`DR CONTRACT_LIABILITY / CR REVENUE`, same stream both legs), atomically with the segment stamp and the SSI-serialized `recognized_minor` in-place delta, at-most-once per `(tenant, RECOGNITION, schedule_id:segment_no)`, guarded by the per-schedule over-recognition CHECK, ordered per schedule (QUEUED + 202 on out-of-order periods), gated on obligation satisfaction with the fail-safe stale-state default, period-assigned per, with reversal as a new compensating entry under `schedule_id:segment_no:reversal` and residual cent to the last segment of the owning version.

**Implements**:
- `cpt-cf-bss-ledger-flow-recognition-run`
- `cpt-cf-bss-ledger-algo-recognition-segment-release`
- `cpt-cf-bss-ledger-state-recognition-segment`
- `cpt-cf-bss-ledger-state-recognition-run`

**Touches**:
- API: `POST /v1/ledger/recognition-runs`
- DB: `recognition_run`, `recognition_segment`, `recognition_schedule`, `account_balance`, `journal`, `journal_line`, `idempotency_dedup`
- Entities: `RecognitionRun`, `RecognitionSegment`

### PO and SSP Gates

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-asc606-recognition-po-ssp`

The system **MUST** enforce PO/allocation-group tagging via a Foundation post-hook owned by this feature (trigger conditions + R4 exemption here; presence gate on the Foundation endpoint), auto-tag the Catalog default so routine lines never block, block only genuinely ambiguous deferred/multi-PO/VC lines (`MISSING_PO_ALLOCATION_GROUP`), require inception-pinned SSP snapshots for multi-PO allocation (`SSP_SNAPSHOT_REQUIRED` + exception queue + Finance alert), accept documented pricing for single-PO lines, and require documented VC estimate + method refs (no VC true-up posting in MVP — successor PRD).

**Implements**:
- `cpt-cf-bss-ledger-algo-recognition-po-ssp-allocation`

**Touches**:
- API: Foundation invoice-post endpoint (additive post-hook problem codes)
- DB: `recognition_schedule` (`po_allocation_group`, `ssp_snapshot_ref`, `vc_estimate_ref`, `vc_method_ref`), `exception_queue`
- Entities: `RecognitionSchedule`

### Revenue-Stream Disaggregation

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-asc606-recognition-disaggregation`

The system **MUST** derive the usage/recurring/one-time stream per revenue-affecting line, split multi-stream bundles into one deferred Contract-liability line and one schedule per stream, resolve each stream to a distinct `account_id` for `REVENUE` and `CONTRACT_LIABILITY` (R6 CoA pattern), keep DR/CR streams equal on every recognition entry, and drain per-stream Contract liability to zero with the per-stream no-negative CHECK as the authoritative drain guard.

**Implements**:
- `cpt-cf-bss-ledger-algo-recognition-disaggregation-split`

**Touches**:
- API: `GET /v1/ledger/revenue/disaggregation`
- DB: `recognition_schedule` (`revenue_stream`), `account_balance`, `tenant_account`
- Entities: `RecognitionSchedule`

### Schedule Change Control

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-asc606-recognition-schedule-change`

The system **MUST** apply upstream-decided changes only via new schedule versions (new `schedule_id`, lineage `version`) or compensating entries — never rewriting released amounts — with approval/dual-control and audit trail, route `catch_up` and unmarked treatments to `MODIFICATION_TREATMENT_REVIEW`, apply `prospective`/`separate_contract` directly, support the S3 credit-note prospective reduction of `total_deferred_minor` over the unreleased remainder, and mark schedules `CANCELLED`/`REPLACED` per the upstream decision.

**Implements**:
- `cpt-cf-bss-ledger-flow-recognition-schedule-change`
- `cpt-cf-bss-ledger-state-recognition-schedule`

**Touches**:
- API: `POST /v1/ledger/recognition-schedules/{scheduleId}/changes`
- DB: `recognition_schedule`, `recognition_segment`, `exception_queue`
- Entities: `RecognitionSchedule`

### Audit Linkage and Invariants

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-asc606-recognition-audit-linkage`

Every recognition journal entry **MUST** carry: (1) the recognition **period/segment**, (2) the **PO/allocation group**, (3) the **deferral origin** — the posted invoice item(s) that posted the Contract liability, and (4) **subscription/entitlement context** when subscription-scoped. **Period alone is insufficient.** A missed-close release additionally records the **original** target period. At schedule-build time **and re-asserted at run time**, `source_invoice_item_ref` MUST **resolve** to an existing posted `journal_line` of class `CONTRACT_LIABILITY` for that schedule — resolution is against `journal_line.invoice_item_ref`, served by the Foundation index `(tenant_id, invoice_id, invoice_item_ref)`; NOT NULL is necessary but not sufficient; failure → `RECOGNITION_WITHOUT_INVOICE_LINK` (422) — a recognition entry without a posted-invoice-item link MUST block (it would imply contract-asset accounting, out of scope, PRD). Cumulative releases remain consistent with the posted Contract liability (within rounding), verified by the Foundation/slice 7 tie-out.

**Implements**:
- `cpt-cf-bss-ledger-algo-recognition-segment-release`
- `cpt-cf-bss-ledger-algo-schedule-builder`

**Touches**:
- DB: `recognition_schedule` (`source_invoice_item_ref`), `journal_line` (`invoice_item_ref` resolution)
- Entities: `RecognitionSchedule`, `JournalLine`

## 6. Acceptance Criteria

Testing is a **delta over the Foundation testing architecture** (same levels + mocking rules; "what must NOT be mocked" + concurrency policy + barrier-start mechanics inherited).

**Unit:**

- [ ] Deferral-policy precedence (Contract>Catalog>PO-type>billing-model); timing precedence
- [ ] Residual cent → last segment of the owning version
- [ ] Immaterial-exemption threshold; SSP-required decision (multi-PO vs single-PO)
- [ ] Disaggregation stream derivation + mixed-invoice split

**Integration (testcontainers):**

- [ ] S6 release `DR Contract liability / CR Revenue` with segment stamp + `recognized_minor` increment **atomic in one txn**
- [ ] **No double recognition**: same segment/period twice → one credit via UNIQUE + idempotency + `status=DONE`
- [ ] **Over-recognition blocked at the per-schedule CHECK even when a sibling schedule keeps the account aggregate positive**
- [ ] Recognition **without** a resolvable invoice-item link blocked
- [ ] Schedule change posts a new version / compensating entry, never rewrites released amounts
- [ ] Multi-stream bundle → per-stream Contract liability drains to zero
- [ ] Deferred-balance-without-schedule surfaced as an exception; recognition reversal decrements `recognized_minor`
- [ ] **Duplicate schedule build** (redelivered/raced) lands on the existing ACTIVE schedule via the partial UNIQUE — never a second `schedule_id`
- [ ] Release posts with the segment's `period_id` while OPEN; a missed-close segment posts to the current open period with original-period linkage
- [ ] Schedule build beyond 120 segments fails the post *(pre- baseline; the degrade strategy supersedes hard failure once confirmed — §11.2)*

**API:**

- [ ] RFC 9457 mapping for each problem code; recognition-run idempotent-replay
- [ ] Out-of-order period returns 202 with body token `recognition-period-queued`

**Ordering & exception:**

- [ ] Period N before → segment `QUEUED`, resumes after predecessor commits
- [ ] Collections-suspended payer still recognizes; cancelled obligation stops/changes per upstream

**Concurrency (Foundation lock-order extended):**

- [ ] Two runs releasing different segments of one schedule whose sum exceeds total → exactly one fails (per-schedule serialization + CHECK)
- [ ] Overlapping different runIds for the same period serialize via the single-active-run guard and never double-credit a segment

**NFR verification:**

- [ ] Recognition-run window → batch timing per tenant/period, sized at the 120-segment schedule bound
- [ ] Write p95 → inherited Foundation load test
- [ ] Recognition-double-credit + over-recognition alarms fire

## 7. Non-Functional Considerations

- **Performance / NFR mapping**:

| NFR | Mechanism | Status |
|-----|-----------|--------|
| Recognition run window (S6, per ledger-owner per period) | Batched per-segment posts; partition-pruned schedule scan; ≤ 120 segments/schedule bound | **Committed 2026-06-18: ≤ 30 min per ledger-owner per period close** within the ≤ 120-segments/schedule bound, gated by the **B3** load test (same gate as the B11 read/write/throughput SLOs) |
| Single-entry write p95 ≤ 500 ms | Inherited Foundation (recognition post is one balanced entry) | Inherited (B2 resolved via B11; B3 load test remains open) |
| Availability ≥ 99.9% / immutability | Inherited Foundation | Inherited |

- **Security**: Inherits Foundation: RLS, append-only, PII-minimized events. Triggering runs and applying controlled schedule changes require the billing-poster / finance scope; schedule changes follow dual-control per policy (audit trail mandatory). Policy/SSP/VC snapshots are read by versioned ref only.
- **Observability**: Metrics: `ledger_recognition_run_duration_seconds` (histogram), `ledger_revenue_recognized_minor{stream}`, `ledger_recognition_double_credit_total`, `ledger_over_recognition_total`, `ledger_recognition_period_queue_depth`, `ledger_schedule_active_total` **(— counts only in-flight `ACTIVE`, excludes terminal `COMPLETED`)**, `ledger_unscheduled_contract_liability_total` (deferred-without-schedule exception). Thresholds wire to the NFR targets + the double-credit/over-recognition alarms.
- **Data**: All tables tenant-scoped RLS (C1); full schemas in §9; `COMPLETED` schedules + segments archivable/partitionable.
- **Compliance**: ASC 606 — inception-pinned SSP allocation (no re-allocation on later SSP changes), modification treatments per ASC 606-10-25-13(a)/(b), accrual-based recognition independent of collections.

## 8. REST API Surface

### 8.1 Endpoints

REST per `rest-api-design`, behind the inbound API gateway.

| Method | Path | Purpose | Idempotency |
|--------|------|---------|-------------|
| `POST` | `/v1/ledger/recognition-runs` | Trigger a recognition run for a period (also schedulable). | per-run-trigger `(tenant, period_id, runId)` at the orchestration layer; each released segment is independently at-most-once via the Foundation gate |
| `GET` | `/v1/ledger/recognition-schedules/{scheduleId}` | Read a schedule + segments + recognized-to-date. | — |
| `POST` | `/v1/ledger/recognition-schedules/{scheduleId}/changes` | Apply an upstream-decided change/cancel/replace (controlled). Was `{scheduleId}:change` — colon custom methods don't route on axum 0.8 / matchit 0.8.4 (dynamic suffix unsupported). | per change id |
| `GET` | `/v1/ledger/revenue/disaggregation` | Recognized revenue by stream/period. | cache/query |

### 8.2 Queued Semantics (202)

**Success / queued semantics (NOT problem+json):** an out-of-order period returns **`202 Accepted`** with body status token `recognition-period-queued` (kebab-case; no SCREAMING_SNAKE error code — uniform across slices 2/3/4) plus segment `QUEUED` + correlation handle; recognition-run replay returns the prior run reference (Foundation AC #19).

### 8.3 Problem Responses (RFC 9457)

True errors only:

| Code | HTTP status | Meaning |
|------|-------------|---------|
| `RECOGNITION_WITHOUT_INVOICE_LINK` | 422 | Recognition without a resolvable posted-invoice-item link (would imply contract-asset accounting, out of scope) |
| `OVER_RECOGNITION` | 409 | Per-schedule `recognized_minor` CHECK failure |
| `MISSING_PO_ALLOCATION_GROUP` | 422 | Additive on the Foundation post endpoint (post-hook); only genuinely ambiguous deferred/multi-PO/VC lines |
| `SSP_SNAPSHOT_REQUIRED` | 422 | Multi-PO without committed SSP snapshot (R3); also opens an exception-queue row + Finance alert |

## 9. Data Model (Slice-Owned Tables)

Adds `recognition_schedule`, `recognition_segment`, `recognition_run`; tenant-scoped RLS (C1).

### 9.1 recognition_schedule

| Column | Type | Notes |
|--------|------|-------|
| `schedule_id` | uuid | PK part; a re-version mints a **new** `schedule_id` |
| `tenant_id` | uuid | PK part |
| `payer_tenant_id` | uuid | |
| `source_invoice_id` | string | |
| `source_invoice_item_ref` | string | NOT NULL; MUST resolve to a posted `CONTRACT_LIABILITY` line via `journal_line.invoice_item_ref` |
| `po_allocation_group` | string | |
| `subscription_ref` | string | nullable |
| `revenue_stream` | enum | single stream per schedule |
| `currency` | char | |
| `total_deferred_minor` | bigint | |
| `recognized_minor` | bigint | `CHECK (recognized_minor <= total_deferred_minor)` — the authoritative over-recognition guard |
| `policy_ref` | string | deferral+timing policy version (immutable) |
| `ssp_snapshot_ref` | string | nullable; multi-PO only |
| `vc_estimate_ref` | string | nullable; variable consideration |
| `vc_method_ref` | string | nullable |
| `status` | enum | `ACTIVE` \| `COMPLETED` \| `REPLACED` \| `CANCELLED` |
| `version` | bigint | lineage label (release-key distinctness comes from `schedule_id`) |

PK `(tenant_id, schedule_id)`; partial `UNIQUE (tenant_id, source_invoice_id, source_invoice_item_ref, revenue_stream) WHERE status='ACTIVE'` is the **at-most-one-live** guard (one current schedule per business key); `REPLACED` versioning keeps history. Build-idempotency is **decoupled from `status`** — it lives in `idempotency_dedup (tenant, flow=SCHEDULE_BUILD, business_id=source_invoice_id:source_invoice_item_ref:revenue_stream)` (operation-key-vs-row-key split), so a fully-recognized schedule moves to terminal **`COMPLETED`** without opening a duplicate-build hole; `COMPLETED` schedules + segments are **archivable/partitionable**, and the `(invoice_item, stream)` duplicate key stays enforceable **permanently** via `idempotency_dedup`. A schedule is **single-revenue-stream** — a multi-stream bundle yields **one schedule per stream**. The deferred **balance** is the Foundation `CONTRACT_LIABILITY` `account_balance` (per stream); `recognized_minor` tracks cumulative release at the schedule (obligation) grain, updated by atomic in-place delta under the lock order.

### 9.2 recognition_segment

| Column | Type | Notes |
|--------|------|-------|
| `tenant_id` | uuid | PK part |
| `schedule_id` | uuid | PK part |
| `segment_no` | int | PK part; **immutable**; 1:1 with `period_id` |
| `period_id` | string | or milestone ref; `UNIQUE (tenant_id, schedule_id, period_id)` |
| `amount_minor` | bigint | |
| `status` | enum | `PENDING` \| `QUEUED` \| `DONE` |
| `recognized_at` | timestamptz | null until `DONE` |
| `run_id` | uuid | null until `DONE` |

PK `(tenant_id, schedule_id, segment_no)`; `UNIQUE (tenant_id, schedule_id, period_id)`; `segment_no` immutable, 1:1 with `period_id` — **dedup grain ≡ UNIQUE grain** (provably identical). Max **120 segments** per schedule (default, §11.2).

### 9.3 recognition_run

| Column | Type | Notes |
|--------|------|-------|
| `run_id` | uuid | PK part |
| `tenant_id` | uuid | PK part |
| `period_id` | string | PK part |
| `started_at_utc` | timestamptz | |
| `status` | enum | `RUNNING` \| `DONE` \| `FAILED` |

PK `(tenant_id, period_id, run_id)` (tenant-first composite — the RLS/secure convention, uniform with `journal`/`payment`/`dispute`; `period_id` is folded into the key so a client reusing one `run_id` across two periods runs **both**); run-trigger dedup on the same `(tenant_id, period_id, run_id)` + a **single-active-run** advisory lock scoped per `(tenant_id, period_id)` at the orchestration layer.

### 9.4 Cross-Table Constraints and Enum Usage

- `CHECK (recognized_minor <= total_deferred_minor)` on `recognition_schedule` — the **authoritative** in-transaction over-recognition guard; the account-level `CONTRACT_LIABILITY` no-negative `CHECK` is defense-in-depth.
- `source_invoice_item_ref` NOT NULL **and** must resolve to a posted `CONTRACT_LIABILITY` line via `journal_line.invoice_item_ref` (Foundation index `(tenant_id, invoice_id, invoice_item_ref)`).
- `policy_ref` / `ssp_snapshot_ref` / `vc_estimate_ref` / `vc_method_ref` are immutable version refs (historical immutability).
- The `source_doc_type` / idempotency `flow` value `RECOGNITION` and the idempotency-only `SCHEDULE_BUILD` (posts no journal entry of its own, `business_id = source_invoice_id:source_invoice_item_ref:revenue_stream`, so the build dedup is independent of `recognition_schedule.status`) are **Foundation-declared**; this feature uses them. Recognition `business_id = schedule_id:segment_no`; reversal `schedule_id:segment_no:reversal`.
- **Lock order:** `recognition_schedule` + `recognition_segment` get a `table_rank` just below the Foundation balance caches (ordered by `(tenant_id, schedule_id)` then `segment_no`); the recognition post also locks the `CONTRACT_LIABILITY` + `REVENUE` `account_balance` rows in the existing Foundation order. Serialization is SSI (write-write conflict detection + retry), not `FOR UPDATE` row locking.

## 10. Events and Alarms

Success via the Foundation outbox: `billing.ledger.revenue.recognized` (`scheduleId`, `segment`, `period`, `amountMinor`, `revenueStream`), `billing.ledger.revenue.recognition_reversed`, `billing.ledger.schedule.changed`.

Alarms via the separate committed audit/alarm txn: `billing.ledger.invariant.alarm` with `alarmCategory ∈ {recognition-double-credit, over-recognition, recognition-period-queued}` (distinct categories — over-recognition maps to the negative-balance/AC #17 guard, double-credit to the PRD dedup-failure alarm). PII-free.

## 11. Decision Log and Open Items

### 11.1 Risks and Deferred Work

- **SSP source of truth (R3) — resolved 2026-06-10:** the blanket multi-PO gate is lifted; `SSP_SNAPSHOT_REQUIRED` remains the per-post guard for a missing/unresolvable snapshot.
- **Policy matrices (R1/R2/R5/R6) — skeleton ratified 2026-06-10** (precedences + conflict rule: same-dimension → Contract wins; unresolvable ambiguity → block + exception queue; R6 = account-per-stream). The **value tables** are Finance-owned data, filled iteratively without code change.
- **Deferred:** contract-asset / unbilled receivable (separate PRD); ERP export of recognized revenue (slice 7); FX on multi-currency recognition (slice 5 — recognition does not re-lock FX; schedule currency = as posted); VC accrual + period re-estimation + modification accounting (catch-up vs prospective) — named successor PRD "Revenue — VC & contract modifications".

### 11.2 Needs Discussion (R1–R6, NFR)

Inherits Foundation open items (B2 resolved 2026-06-10 via B11 — PRD draft committed as v1 SLOs, gated by the B3 load test; B3 remains open). Feature-specific:

| Item | Decision (default) | Status | Owner |
|------|--------------------|--------|-------|
| **SSP source of truth** | Contract override authoritative, Catalog baseline fallback, Finance-approved versioned snapshots; multi-PO block lifted, `SSP_SNAPSHOT_REQUIRED` stays as per-post guard | ✅ Ratified 2026-06-10 | PM + Finance |
| Deferral-policy matrix + conflict rules | Contract→Catalog→PO-type→billing-model; same-dimension conflict → Contract wins; unresolvable ambiguity → block + exception queue; value tables = Finance-owned data | ✅ Ratified 2026-06-10 (skeleton) | PM + Finance |
| Recognition-timing authority/conflict | Contract→Catalog+PO-type→subscription-state (state drives *when*, never the pattern) | ✅ Ratified 2026-06-10 (skeleton) | PM + Finance |
| Variable-consideration vs multi-PO SSP tie-break | multi-PO **+** VC → SSP path mandatory; single-PO VC → documented estimate + method only | ✅ Ratified 2026-06-10 | Finance |
| Disaggregation attribute names + CoA pattern | streams usage/recurring/one-time as **distinct `account_id` per stream** (REVENUE + CONTRACT_LIABILITY); per-stream `CHECK` becomes the authoritative drain guard; stream names Finance-confirmable | ✅ Ratified 2026-06-10 | Design + Finance |
| Immaterial-one-shot exemption threshold | all three conditions: point-in-time, ≤ 1% invoice total or 100 USD-equiv (lower), SKU-flagged; tenant-configurable | ✅ Ratified 2026-06-10 | Finance |
| Recognition run window | **≤ 30 min per ledger-owner per period close**, within the ≤ 120-segments/schedule bound; gated by the B3 load test (rides the same gate as the B11 read/write/throughput SLOs) | ✅ Committed 2026-06-18 | PM |
| Schedule size bound | **120 segments** default guardrail; over-bound **degrades** (coarser/chunked) rather than failing the post; confirm 120 + degrade strategy vs catalog terms / Finance | ⏳ Needs Discussion — degrade direction set 2026-06-17, validation pending | PM + Finance |
