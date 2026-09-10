Created:  2026-07-07 by Virtuozzo International GmbH
Updated:  2026-07-07 by Virtuozzo International GmbH

<!-- migration-note: converted from the legacy vhp-architecture design slice
     docs/bss/design/DESIGN-billing-ledger-balances-202606091200/03-DESIGN-billing-ledger-payments-allocation-202606091300.md
     to the gears-sdlc design-slice layout (cpt-* sub-IDs, CDSL flows). The original is preserved unchanged in the
     vhp-architecture repository. All Foundation
     engine mechanics (PostingService, IdempotencyGate, BalanceProjector, MoneyModule, FiscalPeriodGuard, commit
     trigger, TieOutJob, outbox relay, total fixed lock order) are specified in ./01-repository-foundation.md and are referenced
     here, not restated. -->

# DESIGN — Payments — Settlement, Allocation & Chargebacks (Slice 2)

<!-- toc -->

- [1. Context](#1-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Naming Conventions and Design-Introduced Names](#15-naming-conventions-and-design-introduced-names)
  - [1.6 Scope and Constraints](#16-scope-and-constraints)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Record Payment Settlement](#record-payment-settlement)
  - [Record Settlement Return](#record-settlement-return)
  - [Allocate Payment to Invoices](#allocate-payment-to-invoices)
  - [Record Chargeback Phase](#record-chargeback-phase)
  - [Grant or Apply Reusable Credit](#grant-or-apply-reusable-credit)
  - [Read Unallocated Balance and Allocations](#read-unallocated-balance-and-allocations)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Allocation Precedence (Mode A)](#allocation-precedence-mode-a)
  - [Per-Payment Money-Out Caps](#per-payment-money-out-caps)
  - [Out-of-Order Queueing](#out-of-order-queueing)
  - [Extended Lock Order, No-Negative and Tie-Out](#extended-lock-order-no-negative-and-tie-out)
- [4. States (CDSL)](#4-states-cdsl)
  - [Pending Event State Machine](#pending-event-state-machine)
  - [Dispute Cycle State Machine](#dispute-cycle-state-machine)
  - [AR Sub-Status State Machine](#ar-sub-status-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Settlement Posting (Pattern A)](#settlement-posting-pattern-a)
  - [Settlement Return Posting](#settlement-return-posting)
  - [Allocation Engine (Pattern B, Mode A)](#allocation-engine-pattern-b-mode-a)
  - [Chargeback Posting](#chargeback-posting)
  - [Credit Application (Wallet)](#credit-application-wallet)
  - [Out-of-Order Queue Persistence](#out-of-order-queue-persistence)
  - [Balance Caches, Lock Order and Tie-Out Extension](#balance-caches-lock-order-and-tie-out-extension)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Functional Considerations](#7-non-functional-considerations)
- [8. REST API Surface](#8-rest-api-surface)
  - [8.1 Endpoints](#81-endpoints)
  - [8.2 Queued Semantics (202)](#82-queued-semantics-202)
  - [8.3 Problem Responses (RFC 9457)](#83-problem-responses-rfc-9457)
- [9. Data Model (Slice-Owned Tables)](#9-data-model-slice-owned-tables)
  - [9.1 payment_settlement](#91-payment_settlement)
  - [9.2 payment_allocation](#92-payment_allocation)
  - [9.3 payment_allocation_refund](#93-payment_allocation_refund)
  - [9.4 unallocated_balance](#94-unallocated_balance)
  - [9.5 reusable_credit_subbalance](#95-reusable_credit_subbalance)
  - [9.6 pending_event_queue](#96-pending_event_queue)
  - [9.7 Cross-Table Constraints and Enum Usage](#97-cross-table-constraints-and-enum-usage)
- [10. Events and Alarms](#10-events-and-alarms)
- [11. Decision Log and Open Items](#11-decision-log-and-open-items)
  - [11.1 Risks and Deferred Work](#111-risks-and-deferred-work)
  - [11.2 Needs Discussion (P1–P10)](#112-needs-discussion-p1p10)

<!-- /toc -->

## 1. Context

### 1.1 Overview

Records the **money side** of the invoice lifecycle: a payment **settling** (funds confirmed) is distinct from a payment **allocating** to specific posted invoices, so AR stays correct for prepayments, partial pay, multi-invoice application, and overpayments (PRD § Posting rules S2). Also covers chargeback/dispute posting, bank/ACH/SEPA settlement returns, and the wallet CreditApplication path. This feature posts **under** the Foundation engine (see 01-repository-foundation.md § Component Model) and activates the account classes the Foundation reserved.

**Traces to**: `cpt-cf-bss-ledger-fr-payment-settlement-vs-allocation`, `cpt-cf-bss-ledger-fr-allocation-precedence`, `cpt-cf-bss-ledger-fr-chargeback-dispute-posting`, `cpt-cf-bss-ledger-fr-negative-balance-invariants`, `cpt-cf-bss-ledger-fr-idempotency-per-flow`, `cpt-cf-bss-ledger-fr-idempotent-replay-contract`, `cpt-cf-bss-ledger-fr-out-of-order-event-handling`, `cpt-cf-bss-ledger-fr-money-rounding-scale`, `cpt-cf-bss-ledger-fr-ar-tie-out`, `cpt-cf-bss-ledger-fr-policy-versioning-immutability`, `cpt-cf-bss-ledger-fr-tenant-isolation-posting`, `cpt-cf-bss-ledger-fr-exception-suspense-handling`, `cpt-cf-bss-ledger-fr-posting-immutability`, `cpt-cf-bss-ledger-nfr-posting-performance`, `cpt-cf-bss-ledger-nfr-availability`

### 1.2 Purpose

Keep AR correct while cash moves independently of invoices: receipt alone MUST NOT move AR (Pattern A lands funds in Unallocated cash); allocation (Pattern B) applies cash to invoices under per-payment money-out caps; chargebacks and returns claw money back without ever editing original journal entries; the wallet path converts unallocated cash into reusable customer credit and applies it to AR with no Cash movement.

Success criteria: every money-out path (allocation, refund, chargeback, return) is serialized and capped at the `payment_settlement` counter row; no balance ever goes negative silently; every flow is idempotent on its business key and replay returns the prior reference (Foundation AC #19); out-of-order events queue durably and never post partial outcomes.

**Requirements**: `cpt-cf-bss-ledger-fr-payment-settlement-vs-allocation`, `cpt-cf-bss-ledger-fr-allocation-precedence`, `cpt-cf-bss-ledger-fr-chargeback-dispute-posting`, `cpt-cf-bss-ledger-fr-negative-balance-invariants`, `cpt-cf-bss-ledger-fr-out-of-order-event-handling`, `cpt-cf-bss-ledger-nfr-posting-performance`

**Use cases**: `cpt-cf-bss-ledger-usecase-ledger-inquiry`, `cpt-cf-bss-ledger-usecase-exception-resolution`, `cpt-cf-bss-ledger-usecase-reconciliation-review`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-bss-ledger-actor-payments-psp` | Emits the upstream facts (PaymentSettled, SettlementReturned, allocation intent, dispute cycle/phase, CreditApplication) by **calling** the §8 REST endpoints (call-driven ingestion — no inbound bus on the post path); owns PSP webhook crypto/verification and chargeback case management |
| `cpt-cf-bss-ledger-actor-finance-ops` | Reads the unallocated pool and allocations; resolves exception-queue items (over-allocated returns, chargeback-on-refunded) |
| `cpt-cf-bss-ledger-actor-revenue-assurance` | Receives no-negative / chargeback-cash-negative / aged-queue alarms (same routing as AC #17 violations) |
| `cpt-cf-bss-ledger-actor-finance-approver` | Dual-control approval for high-value credit grants and chargeback-loss postings (effective-dated policy thresholds) |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — § Posting rules S2, § Balances, § Allocation precedence, § Chargebacks, § Edge cases (CreditApplication), § Money, AC #5/#7/#10/#25
- **Design**: [01-repository-foundation.md](./01-repository-foundation.md) — Foundation engine (per-entry ACID PostingService, append-only journal + strict line-negation reversal, IdempotencyGate + 3-column `idempotency_dedup (tenant_id, flow, business_id)` PK, MoneyModule banker's rounding + residual-cent rules, BalanceProjector upsert + conditional no-negative CHECK, FiscalPeriodGuard, leaf-partition balance/zero-sum commit trigger, total fixed lock order, multi-tenant per-line scoping, daily TieOutJob, outbox relay). Not restated here; RFC 2119 keywords are normative.
- **Dependencies**: Foundation engine / posting-engine-core (slice 1) upstream; Payments module (PSP) upstream. Downstream: adjustments-notes-refunds (slice 3, reuses `payment_allocation` + the per-(payment,invoice) cap), reconciliation-export (slice 7, Payments↔PSP and unallocated visibility).

**Canonical slice numbering** (decomposition order, used throughout): 1 posting-engine-core, **2 payments-allocation (this feature)**, 3 adjustments-notes-refunds, 4 asc606-recognition, 5 fx-multicurrency, 6 audit-immutability-observability, 7 reconciliation-export, 8 other. PRD posting-rule labels (S1…S6) are a **separate** axis from slice numbers.

### 1.5 Naming Conventions and Design-Introduced Names

**Account-class names** map to the **Foundation** `account_class` enum literals: Unallocated cash = `UNALLOCATED`, Cash/clearing = `CASH_CLEARING`, Reusable customer credit = `REUSABLE_CREDIT`, plus `AR`. The Foundation `account_class` enum (which declares every class any handler uses) includes `DISPUTE_HOLD` (debit-normal) — parking for the chargeback **cash-hold** variant (§2 Record Chargeback Phase; GL treatment pending Finance, §11.2 P9) — and `PSP_FEE_EXPENSE` (debit-normal expense) for PSP per-transaction / per-dispute fees. This feature **posts to** them, it does not alter the enum. Prose "suspense/unapplied pool" always means `UNALLOCATED`; the `SUSPENSE` class is mapping-exception parking only and is never posted by this feature. The `source_doc_type` / `flow` enums (Foundation-declared, incl. `INVOICE_POST | REVERSAL | MAPPING_CORRECTION` and the payment flows `PAYMENT_SETTLE | PAYMENT_ALLOCATE | CHARGEBACK | CREDIT_APPLY | SETTLEMENT_RETURN`) carry this feature's values; idempotency `flow` mirrors these. **Dispute vs chargeback:** "dispute" names the case/correlation from Payments; "chargeback" names the posting flow.

Design-introduced names (this feature):

| Name | Meaning |
|------|---------|
| `payment_settlement` | Per-payment guarded counter `(tenant, payment_id, settled_minor, allocated_minor, refunded_minor, refunded_unallocated_minor, clawed_back_minor)` — the **DB serialization point** for every per-payment money-out cap (allocations, refunds, clawbacks, returns); CHECKs in §9.1. |
| `payment_allocation` | Ledger record of an N:M Invoice↔Payment allocation the ledger **computed**; drives per-invoice AR reduction + the per-`(payment,invoice)` cap reused by refunds (slice 3). |
| `payment_allocation_refund` | Per-`(payment, invoice)` guarded counter `(tenant, payment_id, invoice_id, allocated_minor, refunded_minor)` — **created and incremented here at allocation time** (`allocated_minor += amount` in the same ACID txn, multiple allocations to one pair aggregate); slice 3 adds the `refunded_minor` side + its `CHECK (refunded_minor ≤ allocated_minor)` consumption. Migration owned by this feature. |
| `unallocated_balance` | Derived cache of **Unallocated cash** per `(tenant, payer, currency)`. |
| `reusable_credit_subbalance` | Derived cache of **Reusable customer credit** per `(tenant, payer, currency, credit_grant_event_type)` — the per-event-type sub-balance the PRD mandates. |
| `pending_event_queue` | Durable store of queued/quarantined work items — allocation-before-settlement, out-of-order dispute phases, dispute-held refund stage-2, slice 3 refund quarantine. **Owned by this feature; used by slices 2 and 3.** |
| AR sub-status (`ar_status` = `ACTIVE` \| `DISPUTED`) | An **as-posted snapshot** tag on `journal_line` and a **mutable** dimension on the `ar_invoice_balance` cache; chargeback "dispute opened" reclassification is **AR-class-neutral**. |

### 1.6 Scope and Constraints

**Non-goals / out of scope:**

- **PSP card rails, webhook crypto/verification** — Payments module (PRD); the ledger **consumes** events.
- **Refunds (S5)**, credit/debit notes (S3/S4), contra-revenue, refund-clearing → slice 3. This feature never returns cash.
- **Chargeback case management** (alerts, evidence, UX) — Payments (PRD); this feature records only posting + reconciliation linkage.
- **Recognition (S6)**, FX, ERP export — later slices. Single-currency, FX-ready fields only.
- **Engine mechanics** — Foundation (01-repository-foundation.md); not restated.

**In scope:**

- S2 settlement vs allocation + N:M via `PaymentAllocation` (§ Posting rules S2; AC #5)
- partial multi-invoice allocation, overpayment-remainder-stays-unallocated, prepayment-Pattern-A-only
- unallocated/reusable-credit/overpayment semantics, split **not** optional (§ Balances)
- allocation precedence default + tenant overrides incl. customer-instructed (statutory registry deferred; AC #25)
- chargeback opened/won/lost/partial + idempotency-by-phase (AC #10)
- the dispute-opened sub-class move being **AR-class-neutral in the AR tie-out roll-up (AC #7)**
- CreditApplication two shapes + caps + both-grain serialization (§ Edge cases)
- out-of-order allocation-before-settlement & chargeback-phase queueing (§ Out-of-order)
- Payments↔PSP reconciliation visibility (§ Reconciliation flows)
- aged unallocated/clearing-queue alarm (§ Observability)
- money residual-cent for multi-invoice allocation

**Idempotency flows:** settle (per PSP txn id), allocate (per allocation id), chargeback (per `(tenant, dispute_id:cycle:phase)`), settlement return (per PSP return id), credit-apply (per CreditApplication id).

**Consumed upstream facts:** `PaymentSettled` (`pspTransactionId`, amount, payer, currency, **optional `feeMinor`** — PSP per-transaction fee withheld); **allocation intent** (`paymentId`, lump `amount`, payer, currency, **optional** customer-instructed `invoiceId` hint — **no candidate set from Payments**; the ledger derives the candidate open invoices from its **own** `ar_invoice_balance` (oldest-first default) and computes the per-invoice split, Mode A / P5 minimal form, 🔄 2026-06-15); dispute events (`disputeId`, `cycle` — dispute cycle number, starts at 1, increments on re-open, `phase ∈ {opened, won, lost, partial}`, amount); `SettlementReturned` (bank/ACH/SEPA return: `pspReturnId`, origin `pspTransactionId`, amount); `CreditApplication` (`creditApplicationId`, type, amount, payer/invoice). The ledger does **not** verify PSP webhooks (Payments owns that). **(Ingestion model — README):** all the above are **call-driven** — Payments (or its adapter) calls the §8 REST endpoints; the ledger consumes **no** inbound bus on the post path (C3). "Events" here name the upstream fact, not a subscription. **Settlement-event contract gate.** The field list above **is** the ratified **ledger-side** settlement consumer contract; the **other side is owned by Payments** and is a **pre-build gate** — Payments MUST confirm it emits exactly these facts (especially `feeMinor` for, the **PSP funds-movement fact** for the dispute-open variant, `cycle`, `pspReturnId`, and the refund `(pspRefundId, phase)` lifecycle) before this feature is built. Tracked in PRD § Deferred to future scope → Cross-team contract gates.

**Constraints and assumptions** (inherits Foundation constraints C1–C4 and assumptions A1–A6, incl. B2/B3 open NFR items). Feature-specific:

| # | Topic | Assumption (default) | Source |
|---|-------|----------------------|--------|
| P1 | Unallocated-pool modeling | **Single** Unallocated bucket + **per-event-type sub-balances** (`credit_grant_event_type`), preserving the unallocated-vs-reusable-credit split. PRD "suspense/unapplied pool" prose = `UNALLOCATED` only. | PRD |
| P2 | Dispute-opened segregation | **Variant driven by the PSP funds-movement fact, not tenant policy:** PSP **withholds funds at open** (card rails) → **cash-hold mandatory** (to the additive `DISPUTE_HOLD` class) so `CASH_CLEARING` ties to the PSP balance; PSP **does not move funds at open** (invoice/ACH) → **within-AR reclassification** (`ACTIVE→DISPUTED`), AR-class-neutral. won/lost/partial branch on which variant was posted. Original payment JEs never edited. | PRD |
| P3 | Atomic settle-and-apply shortcut | Single-entry `DR Cash / CR AR` shortcut allowed **only** with PSP/tenant-guaranteed atomic settle+apply, no residual unallocated. The shortcut MUST still **seed `payment_settlement`** in the same txn (`settled_minor = allocated_minor = amount`, other counters 0) — otherwise a later refund/chargeback/return on that payment has no `settled_minor` to cap against. Invariant: **no AR-reducing money movement without a `payment_settlement` row.** | PRD |
| P4 | Statutory allocation-rule registry | **Deferred — out of v1 scope.** The customer-instructed override (precedence step 2) is the B2B compliance path; a data-driven jurisdiction→rule registry is a post-MVP extension, added only if Legal names a market whose payers a statutory regime binds. | PRD |
| P5 | Allocation split ownership | **Mode A (minimal form, 🔄 2026-06-15)**: the ledger **derives the candidate open invoices from its own `ar_invoice_balance`** (oldest-first default) and **computes** the per-invoice split via precedence, writing `payment_allocation` rows. The consumed event carries the **settlement** (amount/payer/currency) **+ an optional customer-instructed hint** — **not** a candidate set from Payments and **not** pre-split rows. | PRD AC #25 |
| P6 | Tenant-hierarchy payment delegation | **In scope.** `payer_tenant_id` arrives **already resolved** on the consumed settlement event per the payer-resolution rule (payer = nearest `self_managed` ancestor-or-self of `resource_tenant`; `self_managed` = billing boundary); one payer per entry; allocation runs at that payer's grain. The ledger does **not** resolve payer from the tree — resolution is upstream. **Deferred:** ledger-side re-validation guard (Variant C); settlement transfer / payout / summary invoice. | PRD § Multi-tenant |

All handlers post **through** the Foundation `PostingService` (one ACID txn per entry, upsert balance projection in the extended total lock order, leaf-partition commit trigger, idempotent-replay) — see 01-repository-foundation.md § Component Model.

## 2. Actor Flows (CDSL)

### Record Payment Settlement

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-payment-settlement`

**Actor**: `cpt-cf-bss-ledger-actor-payments-psp`

On `PaymentSettled`, post Pattern A — funds land in **Unallocated cash**, **not** AR; receipt alone MUST NOT move AR — and create/seed the `payment_settlement` counter (`settled_minor`; all other counters `= 0`). Posting legs:

| Line | Side | Account class |
|------|------|---------------|
| Cash / bank clearing — **net** deposited | DR | `CASH_CLEARING` |
| PSP fee withheld (if any) | DR | `PSP_FEE_EXPENSE` |
| Unallocated cash — **gross** settled | CR | `UNALLOCATED` |

**PSP fee.** When `PaymentSettled` carries a per-transaction `feeMinor`, the entry splits as above: DR `CASH_CLEARING` **net**, DR `PSP_FEE_EXPENSE` fee, CR `UNALLOCATED` **gross** — so `CASH_CLEARING` ties to the **net** bank deposit while the payer is relieved at **gross** (the fee never reduces AR). **(🔄 2026-06-15)** In v1 the fee is borne by the **merchant tenant — hardcoded, no config** (all lines share its payer-tenant scope, so zero-sum holds). "Platform absorbs the fee" is a **future feature** (not built in v1): it breaks per-tenant zero-sum → needs a separate platform-scope entry, and would only ship with a Finance/Product decision (and could then become a per-tenant/per-plan config). With **no** `feeMinor` (PSP settles gross / invoices fees monthly), the entry is the original two-line shape and the fee posts as its own entry when charged. The Payments↔PSP reconciliation (slice 7) ties at the **net** payout, the fee leg accounting for gross − net. **Note:** the ledger has **no "must not operate at a loss" invariant** — P&L is not a posting constraint; only **account balances** carry the no-negative rule.

**Success Scenarios**:
- Payment settles; funds land in `UNALLOCATED` at gross; `payment_settlement` counter seeded; `unallocated_balance` upserted
- Settlement with `feeMinor` posts the three-leg split; `CASH_CLEARING` ties to net deposit

**Error Scenarios**:
- Replay of the same `pspTransactionId` returns the prior posting reference (Foundation AC #19), no duplicate entry

**Steps**:
1. [ ] - `p1` - Payments module calls API: POST /v1/ledger/payments (body: pspTransactionId, {amountMinor, currency, scale}, resolved payer_tenant_id per P6, optional feeMinor) - `inst-set-api`
2. [ ] - `p1` - Claim idempotency: `idempotency_dedup (tenant, PAYMENT_SETTLE, pspTransactionId)` via Foundation IdempotencyGate - `inst-set-idem`
3. [ ] - `p1` - **IF** replay: **RETURN** prior posting reference (AC #19) - `inst-set-replay`
4. [ ] - `p1` - **IF** feeMinor present: build three-leg entry (DR `CASH_CLEARING` net, DR `PSP_FEE_EXPENSE` fee, CR `UNALLOCATED` gross); **ELSE** two-leg (DR `CASH_CLEARING` / CR `UNALLOCATED` gross) - `inst-set-legs`
5. [ ] - `p1` - Post one balanced entry through the Foundation PostingService (ACID txn, extended lock order §3) - `inst-set-post`
6. [ ] - `p1` - DB: In the same txn, seed `payment_settlement` (`settled_minor` = gross; all other counters = 0) - `inst-set-seed`
7. [ ] - `p1` - DB: Upsert `unallocated_balance` += **gross** amount via BalanceProjector - `inst-set-upsert`
8. [ ] - `p1` - Emit `billing.ledger.payment.settled` via the Foundation outbox (§10) - `inst-set-event`
9. [ ] - `p1` - **RETURN** 201 posting reference - `inst-set-return`

### Record Settlement Return

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-payment-settlement-return`

**Actor**: `cpt-cf-bss-ledger-actor-payments-psp`

**(normative).** A bank/ACH/SEPA return of a settled payment posts flow `SETTLEMENT_RETURN` (idempotent per `(tenant, SETTLEMENT_RETURN, pspReturnId)`; `source_doc_type = SETTLEMENT_RETURN`): DR `UNALLOCATED` / CR `CASH_CLEARING`, clawing back from the payer's Unallocated pool **first**, and decrements `payment_settlement.settled_minor` in the same txn under the rank-1 row lock.

**Success Scenarios**:
- Return posts, unallocated pool clawed back, `settled_minor` decremented

**Error Scenarios**:
- `allocated_minor` exceeds the remaining settled amount → MUST NOT auto-post; routes to the **exception queue** (slice 7; additive type `SETTLEMENT_RETURN_OVER_ALLOCATED`) for explicit de-allocation/chargeback handling
- Return would drive `CASH_CLEARING` negative (funds already swept) → reuses the chargeback-cash-negative pattern (balanced documented-loss line to `DISPUTE_LOSS_EXPENSE` + alarm)

**Steps**:
1. [ ] - `p1` - Payments module calls API: POST /v1/ledger/payments/{paymentId}/returns (body: pspReturnId, origin pspTransactionId, amount) - `inst-ret-api`
2. [ ] - `p1` - Claim idempotency: `idempotency_dedup (tenant, SETTLEMENT_RETURN, pspReturnId)`; **IF** replay **RETURN** prior reference - `inst-ret-idem`
3. [ ] - `p1` - DB: Lock the `payment_settlement` row (rank-1 counter row within the extended lock order) - `inst-ret-lock`
4. [ ] - `p1` - **IF** `allocated_minor` > remaining settled amount after the return: **RETURN** route to exception queue (`SETTLEMENT_RETURN_OVER_ALLOCATED`) — never auto-post - `inst-ret-overalloc`
5. [ ] - `p1` - Post balanced entry DR `UNALLOCATED` / CR `CASH_CLEARING` (claw back from the payer's Unallocated pool first) - `inst-ret-post`
6. [ ] - `p1` - **IF** the entry would drive `CASH_CLEARING` negative: post the balanced documented-loss line (`DISPUTE_LOSS_EXPENSE`) and raise the chargeback-cash-negative alarm instead of a negative Cash balance - `inst-ret-negcash`
7. [ ] - `p1` - DB: Decrement `payment_settlement.settled_minor` in the same txn - `inst-ret-decrement`
8. [ ] - `p1` - Emit `billing.ledger.settlement.returned` (§10) - `inst-ret-event`
9. [ ] - `p1` - **RETURN** 201 posting reference - `inst-ret-return`

### Allocate Payment to Invoices

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-payment-allocation`

**Actor**: `cpt-cf-bss-ledger-actor-payments-psp`

Computes the per-invoice split (Mode A, `cpt-cf-bss-ledger-algo-allocation-mode-a`) for a lump amount, then posts Pattern B:

| Line | Side | Account class |
|------|------|---------------|
| Unallocated cash | DR | `UNALLOCATED` |
| AR (per invoice) | CR | `AR` |

In the same ACID txn the handler increments `payment_settlement.allocated_minor` by the allocated total; the `CHECK (allocated_minor ≤ settled_minor)` row is the **serialization point** that makes concurrent allocates of the same payment safe even when the payer's pooled `unallocated_balance` is positive from **other** payments (over-cap → `ALLOCATION_EXCEEDS_SETTLED`). Spendable headroom additionally nets Pattern-A refunds: `CHECK (allocated_minor + refunded_unallocated_minor ≤ settled_minor)` — cash already refunded from the pool cannot be allocated (same error). Open AR per invoice reduces only by its allocated amount; **one allocate writes N `payment_allocation` rows — one per invoice** (per-row key `(tenant_id, allocation_id, invoice_id)`), request-idempotent per `allocationId` via `idempotency_dedup (tenant, PAYMENT_ALLOCATE, allocationId)` (replay returns the prior reference, no duplicate rows). The handler also upserts `payment_allocation_refund (tenant, payment_id, invoice_id).allocated_minor += <per-invoice amount>` in the same txn (lock rank per slice 3's unified order), so the slice 3 refund cap always reads an authoritative, race-free counter — never a seed-by-sum at first refund.

**Currency match (normative).** MVP allocation requires `settlement.currency == invoice.currency` (and the entry stays single-transaction-currency); a mismatch rejects with `ALLOCATION_CURRENCY_MISMATCH` (400 — a malformed request, not a balance cap; Slice 2 review #1/#5) — the PRD's "lock on alloc to invoices in another currency" path is **not silently supported**. Cross-currency application requires a future designed conversion event (two same-currency balanced legs + an FX bridge, slice 5 extension; §11.2 P7). The **multi-invoice allocation residual cent attaches to the largest open invoice** (Foundation `MoneyModule`, PRD). Overpayment remainder stays in `unallocated_balance` — **no silent relabel** to reusable credit (needs the wallet grant flow). Narrow single-entry shortcut only under P3 — and even the shortcut seeds `payment_settlement` (`settled_minor = allocated_minor` in the same txn) so refund/chargeback/return caps still apply.

**Bounds (working default, PM confirmed — §11.2 P10).** One allocation transaction **touches** at most **500 invoices** — the bound is on the invoices the split actually pays (each is a posted `AR` leg), NOT on the payer's open-invoice backlog, which is read-only and uncapped. So a payer with thousands of open invoices whose payment reaches only a few of them allocates in one transaction; the ceiling exists to keep the posted entry under the Foundation engine's 1,000-line limit. The cap is configurable (`payments.max_invoices_per_allocation`, default 500, bounded `1..=998` at boot — never past the line ceiling). A split that would touch more than the cap (a lump large enough to pay > cap invoices at once) is today rejected `ALLOCATION_TOO_LARGE`; processing it as **chunked continuations under the same `allocationId`** (deterministic precedence order, one ACID txn per chunk, idempotency row finalizing on the last chunk) is a tracked follow-up. Referenced by the §6/§7 load tests.

**Success Scenarios**:
- Lump amount is split across open invoices oldest-first; AR reduces per invoice; remainder stays unallocated
- Large open-invoice backlog with a payment that touches ≤ cap invoices → allocated in one transaction (the backlog size is not the bound)

**Error Scenarios**:
- Allocation total over the per-payment cap → `ALLOCATION_EXCEEDS_SETTLED` (409)
- Settlement currency ≠ invoice currency → `ALLOCATION_CURRENCY_MISMATCH` (400)
- Matching settlement not yet arrived → **202** `allocation-queued` (never rejected; queued per `cpt-cf-bss-ledger-algo-payment-out-of-order-queueing`)

**Steps**:
1. [ ] - `p1` - Payments module calls API: POST /v1/ledger/payments/{paymentId}/allocations (body: allocationId, lump amount, payer, currency, optional customer-instructed invoiceId hint) - `inst-alloc-api`
2. [ ] - `p1` - Claim idempotency: `idempotency_dedup (tenant, PAYMENT_ALLOCATE, allocationId)`; **IF** replay **RETURN** prior reference (or continuation handle / queued reference) - `inst-alloc-idem`
3. [ ] - `p1` - **IF** no matching `payment_settlement` row (allocation before settlement): queue in `pending_event_queue` and **RETURN** 202 `allocation-queued` + correlation handle - `inst-alloc-queue`
4. [ ] - `p1` - **IF** settlement.currency != invoice.currency: **RETURN** 400 `ALLOCATION_CURRENCY_MISMATCH` - `inst-alloc-currency`
5. [ ] - `p1` - DB: Derive candidate open invoices from the ledger's **own** `ar_invoice_balance` (oldest-first default; Mode A / P5) - `inst-alloc-candidates`
6. [ ] - `p1` - Algorithm: compute per-invoice split using `cpt-cf-bss-ledger-algo-allocation-mode-a` (residual cent → largest open invoice) - `inst-alloc-split`
7. [ ] - `p1` - Enforce the touched-invoice bound on the COMPUTED split (invoices receiving a positive amount), not the candidate read: **IF** the split touches > `payments.max_invoices_per_allocation` (default 500) invoices **RETURN** `ALLOCATION_TOO_LARGE`. Processing such a split as chunked continuations under the same `allocationId` (one ACID txn per chunk, deterministic precedence order, idempotency row finalizing on the last chunk) is a tracked follow-up - `inst-alloc-chunk`
8. [ ] - `p1` - Post one balanced entry per txn: DR `UNALLOCATED` / CR `AR` per invoice, through the Foundation PostingService - `inst-alloc-post`
9. [ ] - `p1` - DB: In the same txn, increment `payment_settlement.allocated_minor`; **IF** `CHECK (allocated_minor + refunded_unallocated_minor ≤ settled_minor)` fails **RETURN** 409 `ALLOCATION_EXCEEDS_SETTLED` - `inst-alloc-cap`
10. [ ] - `p1` - DB: In the same txn, insert N `payment_allocation` rows (one per invoice, key `(tenant_id, allocation_id, invoice_id)`), stamping `precedence_policy_ref` - `inst-alloc-rows`
11. [ ] - `p1` - DB: In the same txn, upsert `payment_allocation_refund.allocated_minor += per-invoice amount` per `(tenant, payment_id, invoice_id)` - `inst-alloc-refundcap`
12. [ ] - `p1` - Emit `billing.ledger.payment.allocated` (§10) - `inst-alloc-event`
13. [ ] - `p1` - **RETURN** 201 posting reference (per-invoice split) - `inst-alloc-return`

### Record Chargeback Phase

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-payment-chargeback-phase`

**Actor**: `cpt-cf-bss-ledger-actor-payments-psp`

Records posting + reconciliation linkage only; idempotent per `(tenant, dispute_id:cycle:phase)` — the `business_id` is the snake_case composite `dispute_id:cycle:phase` occupying the Foundation's single `business_id` column (PK shape `(tenant_id, flow, business_id)` unchanged); `cycle` is the dispute cycle number from the Payments contract (starts at 1, increments when a dispute re-opens, e.g. pre-arbitration/arbitration). The phase state machine is **re-entrant per cycle**: after `won` in cycle N, cycle N+1 posts `opened` again. For the **opened** phase, `dispute_id` is the stable correlation id from Payments (the PSP dispute id, or a `paymentId`-derived correlation where the PSP keys off the payment). Original payment JEs are never edited.

**Variant selection at `opened`:** the `opened` posting picks cash-hold vs AR-reclass from the **PSP funds-movement fact** on the dispute event, **not** tenant policy — if the event reports the disputed funds **withheld** (card rails), `cash-hold` is **mandatory** (`DISPUTE_HOLD`) so `CASH_CLEARING` ties to the PSP balance during the dispute; if funds are **not** moved at open (invoice/ACH), `AR-reclass` (`ACTIVE→DISPUTED`). Outcomes then **branch on the recorded `opened` variant** (P2):

| Phase | If opened = AR-reclass | If opened = cash-hold |
|-------|------------------------|------------------------|
| **opened** | Post a balanced reclassification `AR ACTIVE→DISPUTED` (AR-class-neutral; MUST NOT zero/negate `(payer,invoice)` AR or flip payer-aggregate sign) | Move settled cash `CASH_CLEARING → DISPUTE_HOLD` (additive class; §11.2 P9) |
| **won** | Reclassify `DISPUTED→ACTIVE` (no cash leg) | Release `DISPUTE_HOLD → CASH_CLEARING` |
| **lost** | `DR AR` (re-open) and net funds kept vs clawed back via a balanced Cash/loss line | Release `DISPUTE_HOLD` and `CR CASH_CLEARING` for the clawback |
| **partial / split** | Balanced JEs netting to the PSP outcome | same |

Revenue is unchanged unless an S3 note applies (slice 3). **Negative-Cash interaction:** a clawback that would drive `CASH_CLEARING` below zero (funds already swept) MUST NOT silently post and MUST NOT be hard-rejected losing the dispute outcome — it posts a balanced **documented-loss** line to the named class **`DISPUTE_LOSS_EXPENSE`** (minor — debit-normal expense, additive per C2; **not** the `SUSPENSE` mapping class) and routes to the dedicated **chargeback-cash-negative** alarm (§10, same routing to Revenue Assurance as AC #17 violations) so the outcome is recorded without a negative Cash balance. The dispute-**opened** reclass is **excluded** from the AR tie-out roll-up delta (AR-class-neutral, AC #7). **(minor — partial dispute)** A **partial** chargeback disputes only part of an invoice, but `ar_status` on `ar_invoice_balance` is one enum per invoice. To avoid overstating the disputed amount: the reclassification moves **only the disputed sub-amount** between the invoice's AR `ACTIVE`/`DISPUTED` sub-class balances (same balanced AR-class-neutral move, scoped to the disputed minor amount); the invoice-level `ar_status` flag flips to `DISPUTED` only when the **full** open AR is disputed, otherwise it stays `ACTIVE` alongside a non-zero `DISPUTED` sub-balance. Disputed-amount reporting reads the sub-balance, not the flag.

**Refund interaction (normative).** `lost`/cash-out outcomes increment `payment_settlement.clawed_back_minor` in the posting txn under the rank-1 lock; the total money-out cap `CHECK (refunded_minor + clawed_back_minor ≤ settled_minor)` blocks paying out the same settlement twice. An **open** dispute on a payment **holds that payment's refund stage-2** in `pending_event_queue` (slice 3 consults the dispute state before posting). A `lost` outcome on an **already-refunded** payment MUST NOT auto-post — it routes to the exception queue (slice 7; additive type `CHARGEBACK_ON_REFUNDED`) for manual disposition.

**Success Scenarios**:
- `opened` posts the variant selected by the PSP funds-movement fact; `won`/`lost`/`partial` branch on the recorded variant
- Cycle N+1 posts `opened` again after cycle N `won`

**Error Scenarios**:
- Phase out of order (won/lost before opened) → **202** `dispute-phase-queued` with alert; never a partial outcome
- `lost` on an already-refunded payment → exception queue `CHARGEBACK_ON_REFUNDED`, never auto-posted
- Clawback would drive `CASH_CLEARING` negative → documented-loss line + chargeback-cash-negative alarm

**Steps**:
1. [ ] - `p1` - Payments module calls API: POST /v1/ledger/disputes/{disputeId}/phases (body: cycle, phase, amount, PSP funds-movement fact) - `inst-cb-api`
2. [ ] - `p1` - Claim idempotency: `idempotency_dedup (tenant, CHARGEBACK, dispute_id:cycle:phase)`; **IF** replay **RETURN** prior reference - `inst-cb-idem`
3. [ ] - `p1` - **IF** prerequisite phase missing (e.g. won/lost before opened): queue via `cpt-cf-bss-ledger-algo-payment-out-of-order-queueing` and **RETURN** 202 `dispute-phase-queued` + alert - `inst-cb-ooo`
4. [ ] - `p1` - **IF** phase == opened: select variant from the PSP funds-movement fact — funds withheld → cash-hold (`CASH_CLEARING → DISPUTE_HOLD`); funds not moved → AR-reclass (`ACTIVE→DISPUTED`, AR-class-neutral, scoped to the disputed sub-amount for partial disputes) - `inst-cb-opened`
5. [ ] - `p1` - **IF** phase ∈ {won, lost, partial}: post the balanced outcome entry per the phase/variant table above - `inst-cb-outcome`
6. [ ] - `p1` - **IF** phase == lost or cash-out: DB: increment `payment_settlement.clawed_back_minor` in the posting txn under the rank-1 lock; enforce `CHECK (refunded_minor + clawed_back_minor ≤ settled_minor)` - `inst-cb-clawback`
7. [ ] - `p1` - **IF** payment already refunded: **RETURN** route to exception queue (`CHARGEBACK_ON_REFUNDED`) — never auto-post - `inst-cb-refunded`
8. [ ] - `p1` - **IF** clawback would drive `CASH_CLEARING` negative: post balanced documented-loss line to `DISPUTE_LOSS_EXPENSE` + raise chargeback-cash-negative alarm - `inst-cb-negcash`
9. [ ] - `p1` - Emit `billing.ledger.dispute.recorded` (`cycle`, `phase`) (§10) - `inst-cb-event`
10. [ ] - `p1` - **RETURN** 201 posting reference - `inst-cb-return`

### Grant or Apply Reusable Credit

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-payment-credit-application`

**Actor**: `cpt-cf-bss-ledger-actor-payments-psp`

Wallet path (distinct from S5 refunds — **no** Cash movement). Two shapes:

| Event | Posting | Pre-commit cap(s) |
|-------|---------|-------------------|
| **Grant credit** (elect wallet over cash for an unallocated/overpayment balance) | DR `UNALLOCATED` · CR `REUSABLE_CREDIT` | grant ≤ payer's available `unallocated_balance` for the currency (over-cap → `GRANT_EXCEEDS_UNALLOCATED`) |
| **Apply credit to AR** | DR `REUSABLE_CREDIT` · CR `AR` | **(1)** per-invoice: ≤ invoice open AR (`CREDIT_EXCEEDS_OPEN_AR`); **(2)** per-sub-grain wallet: debit specific `reusable_credit_subbalance` rows in a **deterministic order (oldest `credit_grant_event_type` first)** and require Σ over the chosen sub-grains ≤ their summed available (`CREDIT_EXCEEDS_WALLET`) |

Idempotent per `(tenant, CREDIT_APPLY, creditApplicationId)`. Every posted `REUSABLE_CREDIT` journal line MUST carry `credit_grant_event_type` (two-sided `CHECK`, Foundation): grants stamp the granting event's type, applications stamp the drawn sub-grain's type — keeping the wallet sub-grain rebuildable from line truth. Cap (2) is evaluated against the **specific sub-grain rows being debited**, not a notional aggregate, so a draw that overdraws one event-type sub-grain while the aggregate is positive is **blocked pre-commit** (the sub-grain NO-negative `CHECK` is defence-in-depth). Concurrent CreditApplications serialize at **both** grains via the extended lock order (§3). **Contract liability is never a wallet/refund target.**

**Success Scenarios**:
- Grant converts unallocated cash into wallet credit tagged with the granting event type
- Apply draws wallet sub-grains oldest-event-type-first and reduces invoice AR

**Error Scenarios**:
- Grant over the unallocated pool → `GRANT_EXCEEDS_UNALLOCATED` (409)
- Apply over the invoice open AR → `CREDIT_EXCEEDS_OPEN_AR` (409)
- Apply over the chosen sub-grains' available → `CREDIT_EXCEEDS_WALLET` (409)

**Steps**:
1. [ ] - `p1` - Payments module calls API: POST /v1/ledger/credit-applications (body: creditApplicationId, type ∈ {grant, apply}, amount, payer / invoice) - `inst-cr-api`
2. [ ] - `p1` - Claim idempotency: `idempotency_dedup (tenant, CREDIT_APPLY, creditApplicationId)`; **IF** replay **RETURN** prior reference - `inst-cr-idem`
3. [ ] - `p1` - **IF** type == grant - `inst-cr-grant`
   1. [ ] - `p1` - **IF** grant > payer's available `unallocated_balance` for the currency: **RETURN** 409 `GRANT_EXCEEDS_UNALLOCATED` - `inst-cr-grant-cap`
   2. [ ] - `p1` - Post DR `UNALLOCATED` / CR `REUSABLE_CREDIT`, stamping `credit_grant_event_type` from the granting event on the `REUSABLE_CREDIT` line - `inst-cr-grant-post`
4. [ ] - `p1` - **IF** type == apply - `inst-cr-apply`
   1. [ ] - `p1` - **IF** amount > invoice open AR: **RETURN** 409 `CREDIT_EXCEEDS_OPEN_AR` - `inst-cr-apply-ar`
   2. [ ] - `p1` - DB: Select `reusable_credit_subbalance` rows to debit in deterministic order (oldest `credit_grant_event_type` first) - `inst-cr-apply-order`
   3. [ ] - `p1` - **IF** Σ over the chosen sub-grains > their summed available: **RETURN** 409 `CREDIT_EXCEEDS_WALLET` (evaluated against the specific rows being debited, not the aggregate) - `inst-cr-apply-cap`
   4. [ ] - `p1` - Post DR `REUSABLE_CREDIT` / CR `AR`, stamping each `REUSABLE_CREDIT` line with the drawn sub-grain's `credit_grant_event_type` - `inst-cr-apply-post`
5. [ ] - `p1` - Serialize at **both** grains (sub-grain + invoice) via the extended lock order - `inst-cr-serialize`
6. [ ] - `p1` - Emit `billing.ledger.credit.applied` (§10) - `inst-cr-event`
7. [ ] - `p1` - **RETURN** 201 posting reference - `inst-cr-return`

### Read Unallocated Balance and Allocations

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-flow-payment-balance-inquiry`

**Actor**: `cpt-cf-bss-ledger-actor-finance-ops`

**Success Scenarios**:
- Finance Ops reads the unallocated pool by payer/currency; lists a payment's allocations

**Error Scenarios**:
- Cross-tenant read blocked by RLS (payer-tenant axis; Variant B subtree-RLS for parent reads — §7 Security)

**Steps**:
1. [ ] - `p1` - API: GET /v1/ledger/balances/unallocated (query: payer, currency) — cache read of `unallocated_balance` - `inst-inq-unalloc`
2. [ ] - `p1` - API: GET /v1/ledger/payments/{paymentId}/allocations — DB: read `payment_allocation` by index `(tenant_id, payment_id)` - `inst-inq-allocs`
3. [ ] - `p1` - **RETURN** 200 (balances / allocation rows incl. `precedence_policy_ref`) - `inst-inq-return`

## 3. Processes / Business Logic (CDSL)

### Allocation Precedence (Mode A)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-allocation-mode-a`

**Input**: lump settlement amount (+ optional customer-instructed hint), candidate open invoices from `ar_invoice_balance`, effective-dated tenant precedence policy
**Output**: deterministic per-invoice split (N rows) + the `precedence_policy_ref` that produced it

The ledger computes the order/split (Mode A, P5; AC #25):

1. **Default:** oldest posting date first; ties by smallest `invoice_id`.
2. **Tenant overrides:** customer-instructed hint, highest-amount-first, by tax-jurisdiction, or contract priority.
3. **Statutory appropriation rules** (e.g. UK CCA): **out of v1 scope** — the customer-instructed override (step 2) is the B2B compliance path. A data-driven jurisdiction→rule registry (overriding tenant config, rejecting a contradicting strategy with `STATUTORY_ALLOCATION_CONFLICT`) is a deferred post-MVP extension, added only if Legal names a market whose payers a statutory regime binds.

**Policy versioning (normative).** The tenant precedence strategy is an **effective-dated version**; the version in effect at allocation time is stamped on every `payment_allocation` row as `precedence_policy_ref` (the slice 4 pinned-ref pattern). A policy change never rewrites past splits — re-runs reproduce them from the stamped version. (A future statutory registry, if added per, versions the same way.)

**Policy/decision boundary (normative).** Allocation has two separable concerns: **(a) the decision** — given a lump + open invoices, which invoice gets how much; and **(b) the recording** — posting `DR Unallocated / CR AR` per invoice under the money-out caps / no-negative / residual-cent. **(b) is core ledger duty and stays here.** Mode A keeps the *default* decision (oldest-first + tenant overrides) inline as a ratified convenience (P5), but the decision is an **externalizable seam**: (1) **statutory appropriation rules MUST NEVER live in the ledger** — when the registry arrives it is an **external AR-policy / cash-application component** that reads open-AR from a ledger projection and issues an **explicit per-invoice split**, which the ledger only **validates** (caps/no-negative/`payment_settlement`) and records; (2) **Mode B** (the caller sends a pre-computed split, ledger validates) is the documented escape hatch the design weighed — `POST …/allocations` MAY accept a caller-computed split, validated identically, so jurisdiction-specific law never touches the append-only posting path.

**Steps**:
1. [ ] - `p1` - Resolve the effective-dated tenant precedence policy version at allocation time - `inst-prec-policy`
2. [ ] - `p1` - **IF** customer-instructed hint present: apply it as the step-2 override; **ELSE IF** tenant override configured (highest-amount-first / tax-jurisdiction / contract priority): apply it; **ELSE** default oldest posting date first, ties by smallest `invoice_id` - `inst-prec-order`
3. [ ] - `p1` - **FOR EACH** invoice in precedence order: assign min(remaining lump, invoice open AR); stop at zero remainder - `inst-prec-assign`
4. [ ] - `p1` - Attach the multi-invoice residual cent to the **largest open invoice** (Foundation MoneyModule, PRD) - `inst-prec-residual`
5. [ ] - `p1` - Stamp `precedence_policy_ref` on every produced row - `inst-prec-stamp`
6. [ ] - `p1` - **RETURN** deterministic split (reproducible from the stamped policy version) - `inst-prec-return`

### Per-Payment Money-Out Caps

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-payment-money-out-caps`

**Input**: any money-out posting (allocation, refund stage-1, chargeback clawback, settlement return) referencing a payment
**Output**: capped, serialized counter update or a 409 cap error / exception-queue routing

**(— normative).** `payment_settlement` is the single DB serialization point for **all** per-payment money-out: `allocated_minor` (allocation), `refunded_minor` + `refunded_unallocated_minor` (refund stage-1 increments — slice 3), `clawed_back_minor` (chargeback lost/cash-out), and the `settled_minor` decrement on settlement return. Refund rows (slice 3) carry **mandatory** `payment_id` + `currency` for **both** patterns, so every money-out finds its counter row. **Invariant:** **no AR-reducing money movement may post without a `payment_settlement` row** — every settlement path (Pattern A **and** the P3 atomic shortcut) seeds it, so the serialization point always exists for later refund/chargeback/return caps.

**Steps**:
1. [ ] - `p1` - DB: Lock the `payment_settlement` row for the payment (rank-1 counter within the extended lock order; balance grains first — §3 lock order) - `inst-cap-lock`
2. [ ] - `p1` - Apply the counter delta for the flow (allocated_minor / refunded_minor / refunded_unallocated_minor / clawed_back_minor / settled_minor decrement) in the posting txn - `inst-cap-delta`
3. [ ] - `p1` - Enforce CHECKs post-delta: `allocated_minor ≤ settled_minor`; `allocated_minor + refunded_unallocated_minor ≤ settled_minor`; `refunded_minor + clawed_back_minor ≤ settled_minor`; `refunded_minor ≤ settled_minor`; `refunded_unallocated_minor ≥ 0`; `clawed_back_minor ≥ 0` - `inst-cap-checks`
4. [ ] - `p1` - **IF** a CHECK fails: **RETURN** the flow's cap error (409, e.g. `ALLOCATION_EXCEEDS_SETTLED`) or route to the exception queue where the flow mandates it (over-allocated return, chargeback-on-refunded) - `inst-cap-fail`
5. [ ] - `p1` - **RETURN** committed counter state (journal + counters commit or roll back together) - `inst-cap-return`

### Out-of-Order Queueing

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-payment-out-of-order-queueing`

**Input**: an event whose prerequisite is missing (allocation before settlement; chargeback phase out of order; dispute-held refund stage-2; slice 3 refund quarantine)
**Output**: durable queued item + stable replay reference; later apply or cancel

- **Allocation before settlement:** the allocation **queues** (not rejected) and applies when the matching settlement arrives; aged orphan allocations alarm.
- **Chargeback phase out of order** (won/lost before opened): a missing prerequisite phase **queues** with an alert, never posts a partial outcome.
- **Queued-item lifecycle (normative):** on intake the `idempotency_dedup` row is claimed with status `QUEUED` (so a replay during the queued window returns that stable reference, honouring Foundation AC #19) and finalized to `POSTED` only on apply. For a `QUEUED` replay, `result_entry_id`/`posted_at_utc` are null and the returned reference is the queued correlation handle + status — a deliberate widening of the Foundation reference shape that still satisfies AC #19's prior-reference requirement. The per-payment cap, no-negative, and precedence are re-evaluated **at apply time** against then-current state — a queued item that has become over-cap by apply time is blocked/alarmed, never silently posted.
- **Persistence (normative):** every queued/quarantined item is a row in `pending_event_queue` — `(tenant_id, flow, business_id, payload jsonb (PII-free), queued_at, apply_after (nullable), status QUEUED|APPLIED|CANCELLED, attempts)`, RLS-scoped (C1), indexed `(tenant_id, flow, status, queued_at)`. **Owned by this feature; used by slices 2 and 3** — slice 3's refund quarantine and the dispute-held refund stage-2 persist here. Appliers claim rows `FOR UPDATE SKIP LOCKED` before entering the posting lock order; the aged-queue alarms (§10) read this table. A queued financial event survives restarts and never depends on upstream re-push. **(minor — single source of truth & recovery)** `pending_event_queue` is the **work-state SoT** for a queued item; `idempotency_dedup` only holds the **replay reference** (`QUEUED→POSTED`). The two writes happen in **one txn at intake** (claim dedup `QUEUED` + insert queue row), so a crash leaves either neither or both — never a half state. Apply is a **second txn** that posts and, atomically, flips `idempotency_dedup`→`POSTED` and `pending_event_queue.status`→`APPLIED`; a crash between intake and apply leaves a `QUEUED`/`QUEUED` pair that the applier safely re-drives (idempotent post), and a replay during the window returns the `QUEUED` reference. Reconciliation of the two is: `pending_event_queue.status` drives work, `idempotency_dedup` drives replay — they can lag by one txn but never diverge in outcome.
- *(Deferred: refund-before-payment (S5) and out-of-order recognition (S6) queue rules are owned by slices 3 and 4; slice 3's quarantined payloads persist in `pending_event_queue` above.)*

**Steps**:
1. [ ] - `p1` - Intake txn: claim `idempotency_dedup` with status `QUEUED` **and** insert the `pending_event_queue` row (PII-free payload) atomically - `inst-ooo-intake`
2. [ ] - `p1` - **RETURN** 202 with kebab-case status token (`allocation-queued` / `dispute-phase-queued`) + correlation handle - `inst-ooo-202`
3. [ ] - `p1` - **IF** replay while queued: **RETURN** the stable `QUEUED` reference (null `result_entry_id`/`posted_at_utc`, queued correlation handle + status) - `inst-ooo-replay`
4. [ ] - `p1` - Applier: claim due rows `FOR UPDATE SKIP LOCKED` before entering the posting lock order - `inst-ooo-claim`
5. [ ] - `p1` - Apply txn: re-evaluate per-payment cap, no-negative, and precedence against **then-current** state - `inst-ooo-reeval`
6. [ ] - `p1` - **IF** over-cap at apply time: block + alarm, never silently post - `inst-ooo-blocked`
7. [ ] - `p1` - **ELSE** post via PostingService and atomically flip `idempotency_dedup`→`POSTED`, `pending_event_queue.status`→`APPLIED` - `inst-ooo-apply`
8. [ ] - `p1` - **RETURN** applied/blocked outcome; aged-queue alarms read `pending_event_queue.queued_at` - `inst-ooo-return`

### Extended Lock Order, No-Negative and Tie-Out

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-payment-lock-order-tie-out`

**Input**: every payment-feature post; the daily TieOutJob AR projection
**Output**: deadlock-free serialized posting; extended AR tie-out roll-up

**Extended total lock order (over the Foundation's) — reconciled with the implementation (Slice 2 review #2 / #22).** Every post in this feature acquires its **balance-cache** row locks first, via the Foundation `BalanceProjector`, in one fixed `table_rank` order — `account_balance` → `ar_payer_balance` → `ar_invoice_balance` → `unallocated_balance` → `reusable_credit_subbalance` → `tax_subbalance`, then by `(tenant_id, …key…)` — and **only then** the in-transaction sidecar writes the counter rows (`payment_settlement`, `ledger_dispute`, `payment_allocation_refund`). So the balance grains are locked **before** the counters (the `PostingService` flow is project → sidecar), and within the AR grains **payer is locked before invoice** (`ar_payer_balance` rank < `ar_invoice_balance` rank), matching the projector sort key `(table_rank, tenant_id, account_id, currency, payer_tenant_id, invoice_id)` and the Foundation order. S1 and S2 share **one identical relative order** over the overlapping tables; an architecture test pins the grain ranks so a later slice cannot silently reorder them and open a deadlock cycle. *(The earlier wording — "`payment_settlement` first" and "invoice before payer" — did not match the code and is corrected here; slice 3 MUST follow the order above.)*

**No-negative (NO negative, alarmed per AC #17):** `UNALLOCATED` and `CASH_CLEARING` via the **Foundation's** conditional `account_balance` `CHECK` (the Foundation declares the full guarded set incl. these; this feature posts to them, no cross-slice `ALTER`); **`REUSABLE_CREDIT` follows the `tax_subbalance` pattern** — **no** aggregate `account_balance` `CHECK`, guarded only at the `reusable_credit_subbalance` per-`(currency, event_type)` sub-grain (over-consumption against the credit-elected portion alarms even when the aggregate is positive).

**TieOutJob extension (AC #7):** the AR projection now nets payment **allocations** (Pattern B), CreditApplication **apply-to-AR applications** (— slice 7 wording; wallet application is not a settlement), and chargeback **won/lost/partial** outcomes; the dispute-**opened** sub-class move is AR-class-neutral and MUST NOT enter the AR-delta roll-up. Payments↔PSP: settled cash ties to `CASH_CLEARING`; the unallocated pool is visible and reconciles to PSP/bank net of allocations. **(minor — completeness control)** `pending_event_queue` only covers events that **arrived**; a **lost** `PaymentSettled` (never delivered) is silently-missing cash that queue persistence cannot detect. The **completeness control is the slice 7 Payments↔PSP tie-out** (`CASH_CLEARING` vs PSP net settled): a gap surfaces there as a reconciliation variance, not in this feature.

**Steps**:
1. [ ] - `p1` - Acquire balance-cache row locks first via BalanceProjector in ascending `table_rank` (`account_balance` → `ar_payer_balance` → `ar_invoice_balance` → `unallocated_balance` → `reusable_credit_subbalance` → `tax_subbalance`), then by `(tenant_id, …key…)` - `inst-lock-grains`
2. [ ] - `p1` - Then write the in-transaction sidecar counter rows (`payment_settlement`, `ledger_dispute`, `payment_allocation_refund`) - `inst-lock-counters`
3. [ ] - `p1` - Enforce no-negative: `UNALLOCATED`/`CASH_CLEARING` at the aggregate `account_balance` CHECK; `REUSABLE_CREDIT` only at the sub-grain CHECK - `inst-lock-noneg`
4. [ ] - `p1` - TieOutJob: net allocations + credit apply-to-AR + chargeback won/lost/partial into the AR roll-up; exclude the dispute-opened reclass - `inst-lock-tieout`
5. [ ] - `p1` - **RETURN** (architecture test pins the grain ranks) - `inst-lock-return`

## 4. States (CDSL)

### Pending Event State Machine

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-state-payment-pending-event`

**States**: QUEUED, APPLIED, CANCELLED
**Initial State**: QUEUED (created atomically with the `idempotency_dedup` `QUEUED` claim at intake)

**Transitions**:
1. [ ] - `p1` - **FROM** QUEUED **TO** APPLIED **WHEN** the applier posts the item successfully (second txn; flips `idempotency_dedup`→`POSTED` atomically) - `inst-st-pe-applied`
2. [ ] - `p1` - **FROM** QUEUED **TO** CANCELLED **WHEN** the item is explicitly cancelled/discarded during manual disposition - `inst-st-pe-cancelled`
3. [ ] - `p1` - **FROM** QUEUED **TO** QUEUED **WHEN** apply re-evaluation finds the item over-cap (blocked + alarmed, `attempts` incremented; never silently posted) - `inst-st-pe-blocked`

### Dispute Cycle State Machine

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-state-payment-dispute-cycle`

**States**: opened, won, lost, partial (per cycle; re-entrant per cycle)
**Initial State**: opened (cycle starts at 1; increments on re-open, e.g. pre-arbitration/arbitration)

**Transitions**:
1. [ ] - `p1` - **FROM** opened **TO** won **WHEN** Payments reports phase=won for the cycle (release `DISPUTE_HOLD → CASH_CLEARING` or reclassify `DISPUTED→ACTIVE` per the recorded opened variant) - `inst-st-dc-won`
2. [ ] - `p1` - **FROM** opened **TO** lost **WHEN** Payments reports phase=lost (clawback + `clawed_back_minor` increment; documented-loss line if Cash would go negative) - `inst-st-dc-lost`
3. [ ] - `p1` - **FROM** opened **TO** partial **WHEN** Payments reports phase=partial (balanced JEs netting to the PSP outcome) - `inst-st-dc-partial`
4. [ ] - `p1` - **FROM** won (cycle N) **TO** opened (cycle N+1) **WHEN** the dispute re-opens — the phase machine is re-entrant per cycle; each `(dispute_id:cycle:phase)` is idempotent independently - `inst-st-dc-reopen`
5. [ ] - `p1` - Out-of-order phase arrival (won/lost before opened) does not transition — it queues (`cpt-cf-bss-ledger-state-payment-pending-event`) - `inst-st-dc-ooo`

### AR Sub-Status State Machine

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-state-payment-ar-status`

**States**: ACTIVE, DISPUTED (mutable on the `ar_invoice_balance` cache; **immutable as-posted snapshot** on `journal_line` — never flipped in place, Foundation append-only REVOKE UPDATE/DELETE; every reclassification is a **new balanced entry**)
**Initial State**: ACTIVE

**Transitions**:
1. [ ] - `p1` - **FROM** ACTIVE **TO** DISPUTED **WHEN** dispute opened with the AR-reclass variant **and** the full open AR is disputed; a partial dispute moves only the disputed sub-amount between the ACTIVE/DISPUTED sub-class balances while the flag stays ACTIVE (disputed-amount reporting reads the sub-balance, not the flag) - `inst-st-ar-disputed`
2. [ ] - `p1` - **FROM** DISPUTED **TO** ACTIVE **WHEN** dispute won (reclassify back, no cash leg) - `inst-st-ar-active`
3. [ ] - `p1` - All moves are AR-class-neutral: MUST NOT zero/negate `(payer,invoice)` AR, flip the payer-aggregate sign, or enter the AR tie-out roll-up delta (AC #7) - `inst-st-ar-neutral`

## 5. Definitions of Done

### Settlement Posting (Pattern A)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-payments-allocation-settlement`

The system **MUST** post Pattern A on `PaymentSettled` (funds to `UNALLOCATED`, never AR), split the PSP fee to `PSP_FEE_EXPENSE` when `feeMinor` is present (net Cash / gross Unallocated), seed the `payment_settlement` counter in the same transaction, and upsert `unallocated_balance` — idempotent per `(tenant, PAYMENT_SETTLE, pspTransactionId)`.

**Implements**:
- `cpt-cf-bss-ledger-flow-payment-settlement`
- `cpt-cf-bss-ledger-algo-payment-money-out-caps`

**Touches**:
- API: `POST /v1/ledger/payments`
- DB: `payment_settlement`, `unallocated_balance`, `journal`, `journal_line`, `idempotency_dedup`
- Entities: `PaymentSettlement`

### Settlement Return Posting

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-payments-allocation-settlement-return`

The system **MUST** post `SETTLEMENT_RETURN` (DR `UNALLOCATED` / CR `CASH_CLEARING`) idempotent per `(tenant, SETTLEMENT_RETURN, pspReturnId)`, decrement `settled_minor` under the rank-1 lock, route over-allocated returns to the exception queue (`SETTLEMENT_RETURN_OVER_ALLOCATED`) instead of auto-posting, and reuse the documented-loss pattern for negative-Cash returns.

**Implements**:
- `cpt-cf-bss-ledger-flow-payment-settlement-return`
- `cpt-cf-bss-ledger-algo-payment-money-out-caps`

**Touches**:
- API: `POST /v1/ledger/payments/{paymentId}/returns`
- DB: `payment_settlement`, `unallocated_balance`, `journal`, `journal_line`
- Entities: `PaymentSettlement`

### Allocation Engine (Pattern B, Mode A)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-payments-allocation-allocation`

The system **MUST** derive candidate open invoices from its own `ar_invoice_balance`, compute the split via the precedence algorithm (default oldest-first, tenant overrides, statutory registry deferred), post DR `UNALLOCATED` / CR `AR` per invoice, enforce the per-payment caps at the `payment_settlement` row, write N `payment_allocation` rows stamped with `precedence_policy_ref`, upsert `payment_allocation_refund`, reject cross-currency allocation (400), attach the residual cent to the largest open invoice, bound the number of invoices ONE allocation may touch by `payments.max_invoices_per_allocation` (default 500; a larger split → `ALLOCATION_TOO_LARGE`, with chunked continuations under one `allocationId` a tracked follow-up), and keep overpayment remainders unallocated.

**Implements**:
- `cpt-cf-bss-ledger-flow-payment-allocation`
- `cpt-cf-bss-ledger-algo-allocation-mode-a`
- `cpt-cf-bss-ledger-algo-payment-money-out-caps`

**Touches**:
- API: `POST /v1/ledger/payments/{paymentId}/allocations`, `GET /v1/ledger/payments/{paymentId}/allocations`
- DB: `payment_allocation`, `payment_allocation_refund`, `payment_settlement`, `ar_invoice_balance`, `unallocated_balance`
- Entities: `PaymentAllocation`, `PaymentAllocationRefund`

### Chargeback Posting

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-payments-allocation-chargeback`

The system **MUST** record chargeback phases idempotent per `(tenant, dispute_id:cycle:phase)`, select the opened variant from the PSP funds-movement fact (cash-hold to `DISPUTE_HOLD` vs AR-reclass `ACTIVE→DISPUTED`), branch outcomes on the recorded variant, keep the opened reclass AR-class-neutral and out of the tie-out roll-up, scope partial disputes to the disputed sub-amount, increment `clawed_back_minor` on lost/cash-out under the total money-out cap, hold open-dispute refund stage-2 in the queue, route lost-on-refunded to the exception queue, and post the `DISPUTE_LOSS_EXPENSE` documented-loss line + alarm instead of negative Cash.

**Implements**:
- `cpt-cf-bss-ledger-flow-payment-chargeback-phase`
- `cpt-cf-bss-ledger-state-payment-dispute-cycle`
- `cpt-cf-bss-ledger-state-payment-ar-status`
- `cpt-cf-bss-ledger-algo-payment-money-out-caps`

**Touches**:
- API: `POST /v1/ledger/disputes/{disputeId}/phases`
- DB: `payment_settlement`, `ledger_dispute`, `ar_invoice_balance`, `journal`, `journal_line`, `pending_event_queue`
- Entities: `LedgerDispute`

### Credit Application (Wallet)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-payments-allocation-credit-application`

The system **MUST** support both wallet shapes (grant: DR `UNALLOCATED` / CR `REUSABLE_CREDIT`; apply: DR `REUSABLE_CREDIT` / CR `AR`) with pre-commit caps (`GRANT_EXCEEDS_UNALLOCATED`, `CREDIT_EXCEEDS_OPEN_AR`, `CREDIT_EXCEEDS_WALLET`), stamp `credit_grant_event_type` on every `REUSABLE_CREDIT` line, draw sub-grains oldest-event-type-first against the specific rows debited, serialize at both grains, and never target Contract liability.

**Implements**:
- `cpt-cf-bss-ledger-flow-payment-credit-application`
- `cpt-cf-bss-ledger-algo-payment-lock-order-tie-out`

**Touches**:
- API: `POST /v1/ledger/credit-applications`
- DB: `reusable_credit_subbalance`, `unallocated_balance`, `ar_invoice_balance`, `journal_line`
- Entities: `CreditApplication`

### Out-of-Order Queue Persistence

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-payments-allocation-out-of-order-queue`

The system **MUST** persist every queued/quarantined item in `pending_event_queue` (owned here, used by slices 2 and 3), claim the `idempotency_dedup` row as `QUEUED` atomically at intake, return 202 with kebab-case status tokens, re-evaluate caps/no-negative/precedence at apply time, flip both rows atomically on apply, survive restarts without upstream re-push, and alarm on aged items.

**Implements**:
- `cpt-cf-bss-ledger-algo-payment-out-of-order-queueing`
- `cpt-cf-bss-ledger-state-payment-pending-event`

**Touches**:
- API: `POST /v1/ledger/payments/{paymentId}/allocations` (202 path), `POST /v1/ledger/disputes/{disputeId}/phases` (202 path)
- DB: `pending_event_queue`, `idempotency_dedup`
- Entities: `PendingEvent`

### Balance Caches, Lock Order and Tie-Out Extension

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-payments-allocation-balances-lock-order`

The system **MUST** extend the Foundation total lock order with the payment balance grains and sidecar counters (balance grains before counters; payer before invoice), pin the grain ranks with an architecture test, enforce no-negative per class (`UNALLOCATED`/`CASH_CLEARING` aggregate; `REUSABLE_CREDIT` sub-grain only), and extend the TieOutJob AR roll-up with allocations, credit applications, and chargeback outcomes while excluding the AR-class-neutral dispute-opened move.

**Implements**:
- `cpt-cf-bss-ledger-algo-payment-lock-order-tie-out`

**Touches**:
- DB: `account_balance`, `ar_payer_balance`, `ar_invoice_balance`, `unallocated_balance`, `reusable_credit_subbalance`, `payment_settlement`, `payment_allocation_refund`
- Entities: `BalanceProjector` extension, `TieOutJob` extension

## 6. Acceptance Criteria

Testing is a **delta over the Foundation testing architecture** (same level structure + mocking rules: Unit / Integration-testcontainers / API / E2E; never mock DB constraints, money type, idempotency PK; "what must NOT be mocked" and the concurrency-policy/barrier-start mechanics are inherited).

**Unit:**

- [ ] Allocation precedence: default + tie-break, tenant overrides incl. customer-instructed (statutory registry deferred)
- [ ] Multi-invoice residual cent → largest open invoice
- [ ] Wallet two-shape builders + sub-grain consumption order; cap math

**Integration (testcontainers):**

- [ ] Pattern A then B: settle → unallocated → allocate → AR reduces per invoice; receipt alone does NOT move AR
- [ ] `payment_settlement` CHECK blocks Σ-allocations > settled even when the payer's unallocated pool is positive from other payments
- [ ] Overpayment remainder stays unallocated; `UNALLOCATED`/`CASH_CLEARING` NO-negative CHECK holds
- [ ] `reusable_credit_subbalance` NO-negative at sub-grain: overdrawing one event-type while the aggregate is positive is blocked
- [ ] `payment_allocation` idempotent per `allocationId`; chargeback idempotent per `(dispute_id:cycle:phase)`
- [ ] Chargeback-opened is AR-class-neutral (tie-out roll-up unchanged); `ar_status` reclass posts new lines (no `journal_line` UPDATE)
- [ ] Chargeback-lost never drives `CASH_CLEARING` negative (routes to alarm + balanced loss line)
- [ ] Combined money-out cap: full refund stage-1 then chargeback `lost` on the same payment → second blocked/exception
- [ ] Settlement return decrements `settled_minor`; a return with `allocated_minor` over the remainder routes to the exception queue, never auto-posts
- [ ] Dispute cycle 2 posts `opened` after cycle-1 `won`
- [ ] A >500-invoice allocation chunks under one `allocationId`
- [ ] Queued items survive restart via `pending_event_queue`

**API:**

- [ ] RFC 9457 mapping for each new problem code; settle/allocate idempotent-replay returns the prior reference
- [ ] Queued allocation/phase return **202** with a non-problem body; replay of a `QUEUED` allocationId returns a stable reference

**Ordering & exception:**

- [ ] Allocation-before-settlement queues then applies; a queued allocation that becomes over-cap by apply time is blocked/alarmed
- [ ] Chargeback phase out-of-order queues, never posts a partial outcome; aged-queue/aged-unallocated alarms fire

**Concurrency (Foundation lock-order + policy inherited):**

- [ ] Concurrent allocates of the same payment whose sum exceeds settled → exactly one succeeds (on the `payment_settlement` row lock)
- [ ] Concurrent CreditApplications for the same payer wallet serialize at both grains (sub-grain + invoice) — no wallet over-draw, no AR over-pay
- [ ] Cross-table deadlock-freedom for credit apply interleaved with Pattern B allocation on the same payer/invoice under the extended lock order; barrier-synchronized N tasks then assert invariants

**NFR verification:**

- [ ] Write p95 ≤ 500 ms → load/integration
- [ ] Aged-unallocated + aged-queue + chargeback-cash-negative alarms → the ordering/exception alarm tests
- [ ] `payment_settlement` / `unallocated_balance` hot-row load test (same profile as the Foundation `ar_payer_balance`)

## 7. Non-Functional Considerations

- **Performance**: Inherits Foundation targets — B2 resolved 2026-06-10 via B11 (PRD draft committed as v1 SLOs, gated by the B3 load test); B3 remains open (§11.2). Feature-specific: settle/allocate/credit posts are single-balanced-entry writes (p95 ≤ 500 ms target); hot rows are `payment_settlement` (per payment) and `unallocated_balance` (per payer) — same contention profile + mitigation as the Foundation `ar_payer_balance`. Load tests run at the ≤ 500 invoices/allocation-txn bound. Aged-unallocated and aged-queue ages MUST alarm.
- **Security**: Inherits Foundation RLS. **Tenancy axis & delegation.** The RLS/owner axis is `tenant_id` (= the **resolved payer** per the rule, P6); `payer_tenant_id`/`resource_tenant_id` on `payment_allocation` are **descriptive** dimensions, not the RLS key. **Payment delegation is intra-payer-tenant:** because AR is consolidated onto the resolved payer **at posting time** (direct-to-payer), a parent settling/allocating a managed child's consumption operates entirely within `tenant_id = payer` — **flat RLS is correct for it**, no cross-tenant access. Cross-tenant *reads* (a parent reading a self-managed child's own ledger) are the separate path served by the Variant B subtree-RLS — **ACTIVE in MVP (Slice 6, decision 2.B):** middleware resolves `app.subtree_ids` per request (`BarrierMode::Respect`), so a parent reads its own Respect-subtree; cross-barrier reads stay elevated. No-mixed-payer/legal-entity per entry; append-only; PII-minimized events. Posting payments/disputes/credit requires the billing-poster scope; high-value credit grants and chargeback-loss postings follow dual-control per policy (thresholds are effective-dated policy versions). The ledger trusts Payments-module event provenance (PSP verification upstream, C4).
- **Observability**: Metrics: `ledger_payment_settle_total`, `ledger_settlement_return_total`, `ledger_allocation_total`, `ledger_unallocated_balance_minor` (gauge, by tenant), `ledger_aged_unallocated_seconds`, `ledger_dispute_recorded_total{phase}`, `ledger_chargeback_cash_negative_total`, `ledger_credit_apply_total` / `ledger_credit_apply_rejected_total{reason}`, `ledger_allocation_queue_depth`, `ledger_dispute_phase_queue_depth`. Thresholds wire to the NFR targets and the aged-queue + chargeback alarms.
- **Data**: All new tables tenant-scoped with RLS (C1); full schemas in §9.
- **Compliance**: Single-bucket unallocated pool is compliant only with per-event-type sub-balance tracking + the sub-grain guard (P1); statutory allocation compliance path is the customer-instructed override.

## 8. REST API Surface

### 8.1 Endpoints

REST per `rest-api-design`, behind the inbound API gateway; money as `{amountMinor, currency, scale}`. Mutating posts idempotent on the listed business key.

| Method | Path | Purpose | Idempotency |
|--------|------|---------|-------------|
| `POST` | `/v1/ledger/payments` | Record settlement (Pattern A) + seed counter. Sub-resource create, not `:settle`. | `(tenant, PAYMENT_SETTLE, pspTransactionId)` |
| `POST` | `/v1/ledger/payments/{paymentId}/returns` | Record a bank/ACH/SEPA settlement return. Was `:return`. | `(tenant, SETTLEMENT_RETURN, pspReturnId)` |
| `POST` | `/v1/ledger/payments/{paymentId}/allocations` | Allocate a **lump** amount (+ **optional** customer-instructed hint; ledger derives candidates from its own `ar_invoice_balance`, ≤ 500 invoices per txn) and computes the split. Was `:allocate`; pairs with the GET below. | per `allocationId` |
| `POST` | `/v1/ledger/disputes/{disputeId}/phases` | Record a chargeback phase outcome (`cycle` in body). Was `:record`. | `(tenant, dispute_id:cycle:phase)` |
| `POST` | `/v1/ledger/credit-applications` | Grant or apply reusable customer credit. | per `creditApplicationId` |
| `GET` | `/v1/ledger/balances/unallocated` | Read unallocated pool by payer/currency. | cache read |
| `GET` | `/v1/ledger/payments/{paymentId}/allocations` | List allocations of a payment. | — |

### 8.2 Queued Semantics (202)

**Queued (success-with-deferral, NOT problem+json):** allocation-before-settlement and out-of-order dispute phase return **`202 Accepted`** with a normal body carrying a **kebab-case status token** (`allocation-queued`, `dispute-phase-queued`) + correlation handle — never a SCREAMING_SNAKE error code (deferral convention shared with slices 3/4).

### 8.3 Problem Responses (RFC 9457)

True errors only, added to the Foundation's:

| Code | HTTP status | Meaning |
|------|-------------|---------|
| `ALLOCATION_EXCEEDS_SETTLED` | 409 | Allocation total over the per-payment settled cap |
| `GRANT_EXCEEDS_UNALLOCATED` | 409 | Wallet grant over the payer's available unallocated pool |
| `CREDIT_EXCEEDS_WALLET` | 409 | Credit apply over the chosen sub-grains' available |
| `CREDIT_EXCEEDS_OPEN_AR` | 409 | Credit apply over the invoice open AR |
| `STATUTORY_ALLOCATION_CONFLICT` | 409 | Reserved for the deferred statutory registry |
| `ALLOCATION_CURRENCY_MISMATCH` | 400 | settlement currency ≠ invoice currency; cross-currency application not in MVP |

**(Slice 2 review #1 / #5)** Balance / headroom caps are retriable conflicts on mutable state → **ABORTED (409)**, not 422 — the platform's canonical-error model (AIP-193) exposes no 422; a currency mismatch is a malformed request → **INVALID_ARGUMENT (400)**. Settlement/allocation replay returns the prior posting reference (Foundation AC #19).

## 9. Data Model (Slice-Owned Tables)

Adds `payment_settlement`, `payment_allocation`, `payment_allocation_refund`, `unallocated_balance`, `reusable_credit_subbalance`, `pending_event_queue`; all tenant-scoped with RLS (C1). Migrations owned by this feature.

### 9.1 payment_settlement

The per-payment money-out serialization point.

| Column | Type | Notes |
|--------|------|-------|
| `tenant_id` | uuid | PK part |
| `payment_id` | string | PK part; PSP settlement id |
| `currency` | char | |
| `settled_minor` | bigint | decremented by `SETTLEMENT_RETURN` |
| `allocated_minor` | bigint | incremented by allocation |
| `refunded_minor` | bigint | refund stage-1, slice 3 |
| `refunded_unallocated_minor` | bigint | Pattern-A refund stage-1, slice 3 |
| `clawed_back_minor` | bigint | chargeback lost or cash-out, this feature |

PK `(tenant_id, payment_id)`. CHECKs: `allocated_minor <= settled_minor` (existing, stays); `allocated_minor + refunded_unallocated_minor <= settled_minor` (Pattern-A spendable headroom); `refunded_minor + clawed_back_minor <= settled_minor` (total money-out cap); `refunded_minor <= settled_minor` (kept — incremented by slice 3 stage-1, decremented by its stage-1 reversal); `refunded_unallocated_minor >= 0`; `clawed_back_minor >= 0`.

### 9.2 payment_allocation

| Column | Type | Notes |
|--------|------|-------|
| `allocation_id` | uuid | request-level allocation id (one request → N rows) |
| `tenant_id` | uuid | RLS axis (resolved payer) |
| `payer_tenant_id` | uuid | descriptive dimension, not the RLS key |
| `payment_id` | string | |
| `invoice_id` | string | |
| `amount_minor` | bigint | |
| `currency` | char | |
| `precedence_policy_ref` | string | policy version that produced the split |
| `allocated_at_utc` | timestamptz | |

 Per-row key is **`UNIQUE (tenant_id, allocation_id, invoice_id)`** — one allocate request produces **N rows** (one per invoice), so the request-level `allocationId` is **not** unique by itself; request-level idempotency lives in `idempotency_dedup (tenant, PAYMENT_ALLOCATE, allocationId)`, and chunked continuations append rows for new invoices under the same `allocationId` without collision. Indexes `(tenant_id, payment_id)`, `(tenant_id, invoice_id)`. INSERT-only (unique `allocation_id`) — takes **no** lock rank.

### 9.3 payment_allocation_refund

 Created by this feature's migration.

| Column | Type | Notes |
|--------|------|-------|
| `tenant_id` | uuid | PK part |
| `payment_id` | string | PK part |
| `invoice_id` | string | PK part |
| `allocated_minor` | bigint | incremented by AllocationHandler in the allocation txn (first-touch upsert) |
| `refunded_minor` | bigint | defaults 0; consumed by slice 3, which adds the `CHECK (refunded_minor ≤ allocated_minor)` usage |

PK `(tenant_id, payment_id, invoice_id)`. Its lock rank is the **last** rank in the unified order (after `invoice_exposure`, per slice 3) — this feature acquiring it last is consistent because it never locks the recognition/exposure tables.

### 9.4 unallocated_balance

| Column | Type | Notes |
|--------|------|-------|
| `tenant_id` | uuid | |
| `payer_tenant_id` | uuid | |
| `currency` | char | |
| `balance_minor` | bigint | credit-normal; `CHECK (balance_minor >= 0)` — NO negative |

### 9.5 reusable_credit_subbalance

| Column | Type | Notes |
|--------|------|-------|
| `tenant_id` | uuid | |
| `payer_tenant_id` | uuid | |
| `currency` | char | |
| `credit_grant_event_type` | string | per-event-type sub-grain the PRD mandates |
| `balance_minor` | bigint | `CHECK (balance_minor >= 0)` at the `(currency, event_type)` sub-grain — NO negative |

### 9.6 pending_event_queue

 Owned by this feature; written by slices 2 and 3.

| Column | Type | Notes |
|--------|------|-------|
| `tenant_id` | uuid | PK part |
| `flow` | string | PK part |
| `business_id` | string | PK part |
| `payload` | jsonb | PII-free |
| `queued_at` | timestamptz | read by the aged-queue alarms |
| `apply_after` | timestamptz | nullable |
| `status` | string | `QUEUED` \| `APPLIED` \| `CANCELLED` |
| `attempts` | int | |

PK `(tenant_id, flow, business_id)`; RLS (C1); index `(tenant_id, flow, status, queued_at)`.

### 9.7 Cross-Table Constraints and Enum Usage

- `UNALLOCATED`, `CASH_CLEARING`, `DISPUTE_HOLD` are in the **Foundation's** `account_balance` no-negative guarded set (declared there from the start; not `REUSABLE_CREDIT` — sub-grain-guarded), and `DISPUTE_HOLD` (debit-normal, Finance GL-mapping pending — §11.2 P9) + `DISPUTE_LOSS_EXPENSE` (debit-normal expense; documented-loss line for chargeback/return negative-Cash) are in the **Foundation `account_class` enum**. This feature **posts to** them — no cross-slice `ALTER`.
- `ar_status` (`ACTIVE` | `DISPUTED`) — immutable snapshot on `journal_line`, mutable on `ar_invoice_balance`.
- The `source_doc_type` / `flow` values `PAYMENT_SETTLE | PAYMENT_ALLOCATE | CHARGEBACK | CREDIT_APPLY | SETTLEMENT_RETURN` are **Foundation-declared**; this feature uses them. Chargeback `business_id = dispute_id:cycle:phase` (snake_case composite).
- Lock order extended with `table_rank` slots for the **three contended** new tables (`payment_settlement`, `unallocated_balance`, `reusable_credit_subbalance`); `payment_allocation` is INSERT-only and takes **no** lock rank.

## 10. Events and Alarms

> **⚠️ v1 event-layer status (design ↔ code reconciliation).** There is **no event broker / outbox relay in v1** — the whole event layer is parked (publishers are logged no-ops). So **none** of the events below are actually published at runtime in v1; that parking is intentional and applies fleet-wide. Additionally, of the success events listed here, `billing.ledger.payment.settled`, `billing.ledger.payment.allocated`, and `billing.ledger.credit.applied` have **no payload/schema contract in v1 code at all** (the other events ship dormant payload structs + JSON schemas; these three do not) — building their contract is **deferred with the rest of the event layer**. The underlying flows (settle / allocate / credit-apply) are fully implemented and post correctly; only the event emission is deferred.

Success events via the Foundation outbox: `billing.ledger.payment.settled`, `billing.ledger.settlement.returned`, `billing.ledger.payment.allocated`, `billing.ledger.dispute.recorded` (`cycle`, `phase`), `billing.ledger.credit.applied`.

Aged-queue/aged-unallocated and the chargeback-negative-Cash alarm via the **separate committed** audit/alarm transaction (Foundation): `billing.ledger.invariant.alarm` with `alarmCategory ∈ {aged-allocation-queue, aged-unallocated, dispute-phase-queued, chargeback-cash-negative}`. Aged-queue ages read `pending_event_queue.queued_at`. PII-free.

## 11. Decision Log and Open Items

### 11.1 Risks and Deferred Work

- **Hot rows** `payment_settlement` (per payment) and `unallocated_balance` (per payer) mirror the Foundation `ar_payer_balance` contention; same load-test obligation (B3, §11.2).
- **Single-bucket Unallocated pool** (P1; — pool-meaning "suspense" prose retired) is compliant only with per-event-type sub-balance tracking + the sub-grain guard.
- **Deferred:** refunds (S5), revenue/tax restatement (S3) → slice 3; FX on cross-currency allocation → slice 5.

### 11.2 Needs Discussion (P1–P10)

Inherits Foundation open items — B2 resolved 2026-06-10 via B11 (PRD draft committed as v1 SLOs, gated by the B3 load test); B3 bill-run scale remains open (→ PM Team). Feature-specific:

| Item | Decision | Status | Owner |
|------|----------|--------|-------|
| Unallocated-pool modeling | single bucket + per-event-type sub-balances; "suspense" prose = `UNALLOCATED` only | ✅ Accepted default | — |
| Dispute-opened segregation | variant **driven by the PSP funds-movement fact**, **not** tenant policy: funds withheld at open (card rails) → cash-hold **mandatory** (to `DISPUTE_HOLD`, see P9); funds not moved at open (invoice/ACH) → AR reclassification (AR-class-neutral); won/lost branch on the recorded variant | ✅ Ratified 2026-06-10; superseded to PSP-fact-driven via | Finance |
| Atomic settle-and-apply shortcut | allowed only when atomic, no residual | ✅ Accepted default | — |
| Statutory allocation-rule registry | jurisdiction rules override tenant strategy; registry per Design | 🔄 **Amended 2026-06-11: statutory registry deferred — out of v1 scope.** Customer-instructed override is the B2B compliance path; registry added post-MVP only if Legal names a market whose payers a statutory regime binds. Platform default order = **oldest invoice first** stays. (Supersedes the 2026-06-10 "UK CCA only" ratification.) | Legal + PM |
| Allocation split ownership | **Mode A minimal (🔄 2026-06-15)** — ledger derives candidates from its **own** `ar_invoice_balance` (oldest-first) and computes the split via precedence; event carries **settlement + optional customer-instructed hint** (no candidate set from Payments) | ✅ Ratified 2026-06-10, refined 2026-06-15; remaining **action** (not a design blocker): confirm the settlement-event contract (lump + optional hint) with the Payments module at S2 start | PM + Architecture |
| `payment_allocation_refund` ownership | created + `allocated_minor` maintained **here** at allocation time; slice 3 consumes | ✅ Proposed default | — |
| Cross-currency allocation | **MVP rejects** (`ALLOCATION_CURRENCY_MISMATCH`); future: designed conversion event with FX bridge (slice 5 extension) — PRD "lock on alloc in another currency" deferred, not silently dropped | ✅ **Confirmed 2026-06-17 (@vstudzinskyi, decision 5.A): MVP rejects; cross-currency deferred to slice 5** | PM + Architecture |
| Origin payment reference on money-out events | refund/return rows carry **mandatory** `payment_id` + `currency` for both patterns — assumes every PSP/bank refund/return event references the origin payment | ⏳ Pending — confirm with Payments | Payments team |
| `DISPUTE_HOLD` + `DISPUTE_LOSS_EXPENSE` account classes | chargeback cash-hold parks in `DISPUTE_HOLD`; negative-Cash clawback documented-loss posts to `DISPUTE_LOSS_EXPENSE` | ✅ **Names accepted 2026-06-17 (@vstudzinskyi, decision 4)**; ⏳ Finance to confirm GL **treatment/mapping** | Finance |
| Allocation payload bound | ≤ **500 invoices per allocation txn**; chunked continuation under the same `allocationId` | ✅ **Confirmed 2026-06-17 (@vstudzinskyi, decision 6): 500 invoices/txn** (revisit after B3 load-test) | PM |
