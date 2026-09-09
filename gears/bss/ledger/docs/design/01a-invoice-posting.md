Created:  2026-07-07 by Virtuozzo International GmbH
Updated:  2026-07-07 by Virtuozzo International GmbH

<!-- migration-note: converted from the legacy Virtuozzo DESIGN slice format to the gears-sdlc design-slice layout (cpt-* sub-IDs, CDSL flows/algos/states). Original preserved unchanged at vhp-architecture: docs/bss/design/DESIGN-billing-ledger-balances-202606091200/01a-DESIGN-billing-ledger-invoice-posting-202606091200.md.. -->
<!-- CONFLUENCE_TITLE: [BSS]: Billing Ledger — Invoice-posting handler (Design, Slice 1) -->
<!-- Related: 01-repository-foundation.md (Repository-foundation component model), PRD.md | Upstream: PRD Billing Ledger & Balances, the Repository-foundation | Downstream: payments-allocation, adjustments-notes-refunds, asc606-recognition, fx-multicurrency, reconciliation-export, audit-immutability-observability (sibling feature slices) -->

# DESIGN — Invoice Posting (Slice 1)

<!-- toc -->

- [1. Context](#1-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Scope Boundaries](#15-scope-boundaries)
  - [1.6 Constraints & Assumptions](#16-constraints--assumptions)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Post Invoice Journal Entry](#post-invoice-journal-entry)
  - [Reverse Invoice Entry](#reverse-invoice-entry)
  - [Query Balances, AR Aging & Posted Entries](#query-balances-ar-aging--posted-entries)
  - [Clear Suspense Line (Mapping Correction)](#clear-suspense-line-mapping-correction)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Build Direct-Split Entry](#build-direct-split-entry)
  - [Resolve Account Mapping & Suspense Routing](#resolve-account-mapping--suspense-routing)
  - [Suspense Remap (MAPPING_CORRECTION Re-Post)](#suspense-remap-mapping_correction-re-post)
  - [AR Aging Rollup (Read-Time)](#ar-aging-rollup-read-time)
  - [Reversal Line Negation](#reversal-line-negation)
- [4. States (CDSL)](#4-states-cdsl)
  - [Invoice Journal Entry State Machine](#invoice-journal-entry-state-machine)
  - [Suspense Line State Machine](#suspense-line-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Direct-Split Invoice Posting](#direct-split-invoice-posting)
  - [Account Mapping & Suspense Routing](#account-mapping--suspense-routing)
  - [Strict Line-Negation Reversal](#strict-line-negation-reversal)
  - [AR Balances & Aging Read](#ar-balances--aging-read)
  - [Invoice-Posting API Surface](#invoice-posting-api-surface)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context](#7-additional-context)
  - [7.1 REST API Surface (feature-owned endpoints)](#71-rest-api-surface-feature-owned-endpoints)
  - [7.2 Problem Responses (RFC 9457)](#72-problem-responses-rfc-9457)
  - [7.3 Events Surface](#73-events-surface)
  - [7.4 Data Model (feature-owned tables)](#74-data-model-feature-owned-tables)
  - [7.5 Security & AuthZ](#75-security--authz)
  - [7.6 Feature Metrics](#76-feature-metrics)
  - [7.7 NFR Mapping](#77-nfr-mapping)
  - [7.8 Testing Architecture](#78-testing-architecture)
  - [7.9 Risks, Open Questions & Deferred Work](#79-risks-open-questions--deferred-work)
  - [7.10 Decision Log (Needs Discussion)](#710-decision-log-needs-discussion)
  - [7.11 References](#711-references)

<!-- /toc -->

## 1. Context

### 1.1 Overview

The **first business posting** of the Billing Ledger — the **S1 invoice-post**. This feature builds the balanced **direct-split** journal entry for a posted invoice (DR AR / CR Revenue + Contract-liability + Tax), resolves account mappings, accepts the authoritative tax breakdown as-is, contributes AR balance + aging, and provides the full strict-line-negation reversal flow.

It does **not** implement the posting engine: the atomic balanced journal entry, the append-only `journal_line` truth, the derived balance caches, the universal posting invariants, the total lock order, and the data-access API all live in the **Repository-foundation** (see 01-repository-foundation.md §Component Model — Foundation engine). This feature **calls** that API — it builds balanced lines and calls `postBalancedEntry(lines, flow=INVOICE_POST, businessKey=invoiceId)` — and posts under the Foundation's invariants. The handler **never touches the truth or cache tables directly**; it posts **through** the Foundation's in-process data-access API: `postBalancedEntry`, `applyBalanceDeltas`, `idempotencyClaim`, `pinOpenPeriod`, `applyCounterDelta` (read-then-write ordering is handled by `SERIALIZABLE`/SSI, not a locked-read op). The engine, schema, universal posting invariants, lock order, and projector are **referenced, not redefined**, here.

This feature reuses the PRD glossary (PRD § Glossary, § Accounts) verbatim and inherits the Foundation's implementation glossary (`journal_entry` / `journal_line` = the append-only truth; the balance **caches**; `idempotency_dedup`; **tie-out**). RFC 2119 keywords (MUST / SHOULD / MAY) carry their normative meaning.

**Traces to**: `cpt-cf-bss-ledger-fr-invoice-post-direct-split`, `cpt-cf-bss-ledger-fr-exception-suspense-handling`, `cpt-cf-bss-ledger-fr-reversal-canonical-pattern`, `cpt-cf-bss-ledger-fr-idempotency-per-flow`, `cpt-cf-bss-ledger-fr-idempotent-replay-contract`, `cpt-cf-bss-ledger-fr-account-classes`, `cpt-cf-bss-ledger-fr-money-rounding-scale`, `cpt-cf-bss-ledger-fr-multi-axis-attribution`, `cpt-cf-bss-ledger-fr-tenant-isolation-posting`, `cpt-cf-bss-ledger-fr-audit-retrieval`, `cpt-cf-bss-ledger-nfr-posting-performance`, `cpt-cf-bss-ledger-nfr-availability`

### 1.2 Purpose

Implements **Slice 1 (the invoice-posting handler)** of the Billing Ledger PRD: the ledger **starts at invoice post** — a posted invoice becomes a balanced, immutable, idempotent journal entry with correct AR, Revenue, Contract-liability, and Tax-payable effects, exception-safe account mapping (suspense routing), read-time AR aging, and a strictly governed reversal path. Sibling slices (payments/allocation, credit/debit notes & refunds, ASC 606 recognition, FX, reconciliation/export, audit/observability) are their own feature slices and post **through** the same Foundation.

**Requirements**: `cpt-cf-bss-ledger-fr-invoice-post-direct-split`, `cpt-cf-bss-ledger-fr-exception-suspense-handling`, `cpt-cf-bss-ledger-fr-reversal-canonical-pattern`, `cpt-cf-bss-ledger-fr-idempotency-per-flow`, `cpt-cf-bss-ledger-fr-idempotent-replay-contract`, `cpt-cf-bss-ledger-fr-money-rounding-scale`, `cpt-cf-bss-ledger-nfr-posting-performance`

**Use cases**: `cpt-cf-bss-ledger-usecase-ledger-inquiry`, `cpt-cf-bss-ledger-usecase-exception-resolution`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-bss-ledger-actor-billing-orchestration` | Submits the materialized posted invoice (`InvoiceItem` with `glCode` / `skuId` / `planId` / `priceId` / `pricingSnapshotRef` / revenue stream) via `POST /v1/ledger/journal-entries`; supplies the resolved `payer_tenant_id` |
| `cpt-cf-bss-ledger-actor-tax-engine` | Supplies the authoritative posted `TaxBreakdown` (accepted as-is, never recomputed) |
| `cpt-cf-bss-ledger-actor-catalog-contracts` | Supplies the `glCode` + tax-category account-class mapping snapshot (Catalog) and PO / allocation-group ids (Contracts — presence gate only at S1) |
| `cpt-cf-bss-ledger-actor-finance-ops` | Triggers reversals; performs the dual-control suspense clearing (compensating reversal + `MAPPING_CORRECTION` re-post) |
| `cpt-cf-bss-ledger-actor-finance-approver` | Approver half of the dual-control (preparer/approver, reason code) for reversal, material backdating, and suspense clearing |
| `cpt-cf-bss-ledger-actor-revenue-assurance` | Receives the suspense-backlog aging alarm and the missing-PO alarm; clears exceptions before period close |
| `cpt-cf-bss-ledger-actor-auditor` | Retrieves per-entry who/when/source/correlation via `GET /v1/ledger/journal-entries/{entryId}` (AC #8) |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — posting rules S1, account classes, invariants, ACs
- **Design**: [01-repository-foundation.md](./01-repository-foundation.md) — see 01-repository-foundation.md §Component Model (Foundation engine `postBalancedEntry` / `applyBalanceDeltas`, commit trigger, BalanceProjector, TieOutJob, outbox relay)
- **Dependencies**: Repository-foundation (shared posting engine, schema/DDL, universal invariants, total lock order, data-access API, seller-provisioning endpoint)

### 1.5 Scope Boundaries

**In scope** (PRD requirements covered — the invoice-post domain; the universal invariants those posts run under are Foundation-owned):

- the S1 invoice-post direct-split posting rule (PRD § Posting rules S1; Example A)
- deferred-revenue **credit** at post (AC #4 — credit line only, no release)
- gross AR incl. tax + a separate Tax-payable line per `TaxBreakdown` (B1; tax accepted as-is, never recomputed)
- account-mapping resolution and the missing-mapping route-to-suspense path (A5) + suspense-backlog aging alarm
- AR balances & aging — the S1 contribution to the AR grains and the read-time aging rollup
- the mandatory revenue-stream line tag on Revenue / Contract-liability lines (AC #26 — invariant enforced by the Foundation; this handler populates it; derivation policy deferred to `asc606-recognition`)
- strict line-negation reversal of an invoice entry: at-most-once-per-entry domain semantics, no-reverse-of-a-reversal, amount-correction out of scope
- the invoice-post + reversal **API endpoints** (the handler-facing HTTP surface)

The universal posting invariants (balanced/zero-sum, append-only, ≥1-line, single-payer, no-negative, idempotency + idempotent-replay, money/banker's-rounding, UTC clock + fiscal-period assignment + closed-period/backdating, account-lifecycle posting gates, the minimal period-close transition, the daily tie-out) are **enforced by the Foundation**; this feature builds lines that satisfy them and calls the Foundation API. The account classes this feature posts to — AR, Revenue, Contract liability, Tax payable, and Suspense (for unmapped lines) — are declared in the Foundation's `account_class` enum.

**Out of scope / Non-goals.** Owned by the Foundation, not this feature (referenced, not redefined here):

- the shared posting engine and its per-entry ACID transaction, the `journal_entry` / `journal_line` append-only truth, the balance caches, the universal invariants (balanced/zero-sum, ≥1-line, single-payer, no-negative), the total lock order, and the data-access API.
- the schema / DDL of every shared table (`journal_entry`, `journal_line`, the balance caches, `idempotency_dedup`, `tenant_account`, `fiscal_period`, `currency_scale_registry`, `tenant_posting_lock`, `payer_state`, `event_outbox`). This feature adds **no** shared-table DDL.
- the seller-provisioning endpoint that seeds chart-of-accounts / currency scales / the initial fiscal period.

Deferred to sibling feature slices (the engine provides the seam, not the behavior):

- **S2 payment settlement / allocation**, unallocated cash, overpayment, chargebacks, wallet credit → `payments-allocation`.
- **S3 credit notes / S4 debit notes / S5 refunds**, contra-revenue, refund-clearing → `adjustments-notes-refunds`.
- **S6 recognition runs** and ASC 606 deferral/timing/SSP/disaggregation **policy** → `asc606-recognition`. This feature posts the Contract-liability **credit line** at S1 but runs **no** releases. The revenue-stream classification **invariant** (a line tag MUST be present) is enforced by the Foundation; its **derivation policy** is deferred.
- **FX** realized/unrealized, rate snapshots, stale-rate handling → `fx-multicurrency`. Slice 1 is **single-currency** (transaction currency == functional currency).
- **Reconciliation dashboards and ERP/GL export** → `reconciliation-export`. The daily AR tie-out (correctness control) and the suspense/period-close block are Foundation-owned controls this feature relies on; cross-system export is not in scope.
- **Tamper-evidence mechanism choice**, secured-audit-store internals, GDPR erasure/tombstone, retention/archival → `audit-immutability-observability`.
- **Bad debt / write-off / recovery**, **inter-tenant settlement / reseller payout** — out of scope for the whole PRD (PRD § Out of scope; § Resolved decisions).
- **Rating math / tariff evaluation** — upstream (Metering & Pricing); the ledger starts at invoice post.
- **Historical / as-of (temporal) balance** — `GET /balances` serves only the **current** cache. A balance "as of date T" is **out of S1 scope**; it is reconstructable from `journal_line` (replay to T) when a reporting / reconciliation slice needs it, choosing the basis explicitly — `posted_at_utc` (when recorded, for audit) vs `effective_at` (economic date, for restatement), which diverge for backdated / reversal entries (those post "now" with a past `effective_at`).

**Feature boundary & inputs.** The ledger **starts at invoice post**: this feature consumes a materialized posted invoice + tax evidence + account-mapping snapshot; it does not see raw usage or rating math. **Consumed at S1:** `InvoiceItem` (posted, with `glCode` / `skuId` / `planId` / `priceId` / `pricingSnapshotRef` / revenue-stream); `TaxBreakdown` (accepted as-is, never recomputed); `glCode` + tax-category account-class mapping (Catalog snapshot); PO / allocation-group id (Contracts — consumed only to enforce the *presence* gate; the full SSP / deferral matrix is in `asc606-recognition`). **(Ingestion model — README):** these are **consumed facts delivered via REST calls** from the owning upstream module (or its adapter), **not** events the ledger subscribes to — no inbound bus on the post path (Foundation C3). **Produced:** posted journal entries (via the Foundation); the S1 contribution to AR balance & aging; idempotent-replay posting reference; invariant alarms (raised by the Foundation).

### 1.6 Constraints & Assumptions

**Inherited platform constraints (normative)** are the Foundation's C1–C4 (RLS isolation, backwards-compatible migrations, PostgreSQL is the store, API exposure behind the inbound API gateway). This feature introduces none of its own; it inherits them by posting through the Foundation.

**Blocker assumptions** — recorded with the PRD's own draft direction; each MUST be confirmed by PM Team / Finance before the dependent slice finalizes. These do **not** change the slice-1 journal shape. Only the assumptions exercised by the invoice-post domain are repeated here; the engine-level ones (A2 NFR, A3 tamper, A4 money type) live in the Foundation.

| # | Topic | Assumption (default) | PRD source |
|---|-------|----------------------|------------|
| A1 | Net vs gross tax presentation | **Resolved — gross** (confirmed by Product; B1): S1 posts **gross** AR (incl. tax) + a **separate** Tax-payable line per `TaxBreakdown`. Net / tax-split is an export/inquiry presentation concern, not a journal change. | PRD |
| A5 | Missing account-mapping policy | Default **route-to-suspense**: the line posts to a `SUSPENSE` account class flagged `mapping_status = PENDING`, which **blocks period close** until cleared or an approved exception is recorded (PRD). Tenant-configurable to **hard-block** (`ACCOUNT_MAPPING_MISSING`). Never silent wrong-revenue. | PRD |
| A6 | Material-backdating threshold | Default **5 business days**; valid range **[1..30]**; out-of-range config rejected (no silent clamp). The threshold is evaluated by the Foundation's `FiscalPeriodGuard`; this feature's posts pass `effectiveAt` and are subject to it. | PRD |

## 2. Actor Flows (CDSL)

**Data-flow summary.** A post request enters via the Posting API → the invoice-post handler resolves account mappings (`AccountMappingResolver`), builds the balanced DR AR / CR Revenue + Contract-liability + Tax lines, and calls **`postBalancedEntry(lines, flow=INVOICE_POST, businessKey=invoiceId)`**. Inside that call the Foundation claims idempotency (`idempotencyClaim`), pins/locks the open period (`pinOpenPeriod`), inserts the lines, applies the AR / Revenue / Contract-liability / Tax balance deltas through `applyBalanceDeltas` under its canonical lock order (which also enforces no-negative), and commits under the leaf-partition zero-sum trigger. A reversal request builds the line-negated entry and calls `postBalancedEntry(..., flow=REVERSAL, businessKey=reverses=entryId)`. The handler issues **no** direct balance-row or journal DML — it cannot (REVOKE on shared tables, Foundation-owned).

### Post Invoice Journal Entry

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-invoice-post`

**Actor**: `cpt-cf-bss-ledger-actor-billing-orchestration`

**Success Scenarios**:
- First successful post returns `201` with the posting reference; balances (AR, Revenue, Contract liability, Tax payable) reflect the entry atomically
- Idempotent replay (same key + identical payload) returns `200` with the **prior** posting reference (`entryId`, `postedAtUtc`, `status`) — not an error body
- Missing account mapping under the default tenant policy → line routes to `SUSPENSE` with `mapping_status = PENDING` (post succeeds; period close is blocked until cleared)

**Error Scenarios**:
- Payer `lifecycle_state = CLOSED` → `PAYER_CLOSED` (422)
- Missing mapping and tenant policy = hard-block → `ACCOUNT_MAPPING_MISSING` (422)
- Missing PO / allocation-group tag → post blocked, counted in `ledger_post_blocked_missing_po_total`
- Same key + different payload → `409` `IDEMPOTENCY_PAYLOAD_CONFLICT` (Foundation)
- Any Foundation universal-invariant violation (`LEDGER_ENTRY_UNBALANCED`, `PERIOD_CLOSED`, `MISSING_PAYER`, …) surfaced unchanged

**Steps**:
1. [ ] - `p1` - API: POST /v1/ledger/journal-entries (body: `invoiceId`, lines or invoice items + `taxBreakdown` + revenue-stream, `effectiveAt`, mapping refs, resolved `payer_tenant_id` + `resource_tenant_id` + resolution-provenance stamp) - `inst-ipost-api`
2. [ ] - `p1` - Algorithm: resolve account mappings for every line using `cpt-cf-bss-ledger-algo-invoice-account-mapping` (Catalog snapshot class first, then Contract override; missing mapping → suspense routing or hard-block per tenant policy A5) - `inst-ipost-map`
3. [ ] - `p1` - **IF** tenant policy = hard-block and a mapping is missing **RETURN** 422 `ACCOUNT_MAPPING_MISSING` - `inst-ipost-hardblock`
4. [ ] - `p1` - Algorithm: build the balanced direct-split lines using `cpt-cf-bss-ledger-algo-invoice-line-build` (DR AR gross incl. tax / CR Revenue recognized / CR Contract-liability deferred / CR Tax payable per `TaxBreakdown`) - `inst-ipost-build`
5. [ ] - `p1` - **IF** the PO / allocation-group presence gate fails: block the post, increment `ledger_post_blocked_missing_po_total` **RETURN** error - `inst-ipost-po-gate`
6. [ ] - `p1` - Call Foundation `postBalancedEntry(lines, flow=INVOICE_POST, businessKey=invoiceId)` — inside: `idempotencyClaim` at-most-once per `(tenant, INVOICE_POST, invoiceId)`; `pinOpenPeriod(tenant, legalEntity)` (`fiscal_period FOR SHARE` + `OPEN` assertion; `effectiveAt` subject to closed-period / material-backdating rules A6); line insert; `applyBalanceDeltas` under the canonical lock order with no-negative on the guarded set; commit under the leaf-partition zero-sum trigger - `inst-ipost-foundation`
7. [ ] - `p1` - **IF** the Foundation resolves an idempotent replay (same key + identical payload) **RETURN** 200 with the prior posting reference (`entryId`, `postedAtUtc`, `status`) - `inst-ipost-replay`
8. [ ] - `p1` - **IF** the payer is `CLOSED` (`payer_state.lifecycle_state = CLOSED`, AC #21; Foundation payer-OPEN gate for payer-posting handlers — this handler is one) **RETURN** 422 `PAYER_CLOSED` - `inst-ipost-payer-closed`
9. [ ] - `p1` - **RETURN** 201 Created (posting reference) - `inst-ipost-return`

### Reverse Invoice Entry

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-invoice-reverse`

**Actor**: `cpt-cf-bss-ledger-actor-finance-ops`

**Success Scenarios**:
- A posted invoice entry is reversed exactly once via strict line-negation; the original is untouched; AR/Revenue/Contract-liability/Tax balances net out
- Retried reversal with identical payload replays idempotently (prior reference)

**Error Scenarios**:
- Entry already has a reversal pointing at it → rejected (at-most-once total, even with a different `reason`)
- Entry's `source_doc_type = REVERSAL` → `CANNOT_REVERSE_REVERSAL` (422)
- Referenced original entry does not exist → rejected at post time (lineage is not a DB self-FK)

**Steps**:
1. [ ] - `p1` - API: POST /v1/ledger/journal-entries/{entryId}/reversals (body: `reason` — **payload only**, covered by the Foundation's payload-hash conflict check, never part of the uniqueness key) - `inst-irev-api`
2. [ ] - `p1` - Reversal + material backdating follow **dual-control** per policy (preparer/approver; billing-poster / approver scopes) - `inst-irev-dualcontrol`
3. [ ] - `p1` - **IF** target entry `source_doc_type = REVERSAL` **RETURN** 422 `CANNOT_REVERSE_REVERSAL` — no undo-the-undo chains; a mistaken reversal is corrected by a fresh upstream post (new `invoiceId`), never by reversing the reversal - `inst-irev-no-undo`
4. [ ] - `p1` - Validate the referenced original entry exists at post time (lineage is **not** a DB self-FK — a self-referential FK on a partitioned table prevents detaching/dropping old partitions, breaking 7-year retention) - `inst-irev-exists`
5. [ ] - `p1` - **IF** the entry already has a reversal pointing at it **RETURN** rejection (belt-and-suspenders; the Foundation backs at-most-once-per-entry with the partial unique index on `(tenant_id, reverses_period_id, reverses_entry_id) WHERE reverses_entry_id IS NOT NULL`) - `inst-irev-once`
6. [ ] - `p1` - Algorithm: build the negated lines using `cpt-cf-bss-ledger-algo-invoice-reversal-negation` (same accounts, flipped side, positive amount; carries `reverses_entry_id` + `reverses_period_id`; posts at current effective time) - `inst-irev-negate`
7. [ ] - `p1` - Call Foundation `postBalancedEntry(negatedLines, flow=REVERSAL, businessKey=reverses=entryId)` — dedup key `(tenant, REVERSAL, reverses_entry_id)`, so two reverse calls with different reasons cannot both post - `inst-irev-foundation`
8. [ ] - `p1` - **RETURN** 201 Created (reversal posting reference); idempotent replay returns 200 prior reference - `inst-irev-return`

### Query Balances, AR Aging & Posted Entries

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-invoice-balance-inquiry`

**Actor**: `cpt-cf-bss-ledger-actor-auditor`

**Success Scenarios**:
- Warm balance read from the Foundation's balance cache (filters: `accountClass`, `currency`, `payerTenantId`, optional `invoiceId` for the AR-invoice grain)
- AR aging rollup per payer derived at read time
- Per-entry audit retrieval returns lines + source linkage + actor/correlation (AC #8)
- Paginated journal-line history via cursor pagination

**Error Scenarios**:
- Entry not found (404)
- Cross-tenant access blocked by RLS / tenant context

**Steps**:
1. [ ] - `p1` - API: GET /v1/ledger/balances (filter: `accountClass`, `currency`, `payerTenantId`, optional `invoiceId`) — warm read from the balance cache; returns the **current** cached value only (as-of balance out of S1 scope) - `inst-iq-balances`
2. [ ] - `p1` - API: GET /v1/ledger/balances/ar-aging — Algorithm: derive buckets at read time using `cpt-cf-bss-ledger-algo-ar-aging-rollup` - `inst-iq-aging`
3. [ ] - `p1` - API: GET /v1/ledger/journal-entries/{entryId} — retrieve a posted entry with its lines + source linkage + actor/correlation (AC #8: `posted_by_actor_id`, `origin`, `posted_at_utc`, `source_doc_type` / `source_business_id` / `reverses_entry_id`, `correlation_id`; human-readable PII is **not** stored here) - `inst-iq-entry`
4. [ ] - `p1` - API: GET /v1/ledger/journal-lines — paginated transaction history (cursor pagination; filter: payer, account class, period, source business id) - `inst-iq-lines`
5. [ ] - `p1` - **RETURN** 200 with the requested read (all reads are pure reads over existing caches / truth — no new posted state, no new lock rank) - `inst-iq-return`

### Clear Suspense Line (Mapping Correction)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-flow-invoice-suspense-clearing`

**Actor**: `cpt-cf-bss-ledger-actor-finance-ops`

**Success Scenarios**:
- A `SUSPENSE` line with `mapping_status = PENDING` is cleared by compensating reversal + corrected re-post under flow `MAPPING_CORRECTION`; the period-close block lifts
- A retried half of the two-step operation replays idempotently (deterministic `correction_id`)

**Error Scenarios**:
- Re-post attempted under `INVOICE_POST` → blocked by the surviving dedup row (AC #19 retention rule)
- Approved exception recorded instead of clearing → close-block lifted without re-post

**Steps**:
1. [ ] - `p1` - Revenue Assurance is alerted by the suspense-backlog aging alarm: `ledger_suspense_pending_lines_total` + `ledger_suspense_pending_age_seconds`, warn > N days / page > M days, ticketing Revenue Assurance — so a missing Catalog mapping is caught on the **first** affected invoice rather than as a wall of un-mapped lines at close - `inst-isc-alarm`
2. [ ] - `p1` - Operator executes reversal + corrected re-post as **one dual-control operator action** (preparer/approver, reason code — same governance as material backdating) - `inst-isc-dualcontrol`
3. [ ] - `p1` - Post the compensating reversal of the original entry (flow `REVERSAL`, per `cpt-cf-bss-ledger-flow-invoice-reverse`) - `inst-isc-reverse`
4. [ ] - `p1` - Algorithm: corrected re-post using `cpt-cf-bss-ledger-algo-suspense-remap` (flow `MAPPING_CORRECTION`; the original `(tenant, INVOICE_POST, invoiceId)` dedup row **survives the reversal** and must, per AC #19, so the corrected entry cannot re-use `INVOICE_POST`) - `inst-isc-repost`
5. [ ] - `p1` - The Foundation's `TieOutJob` re-derives the open-PENDING set as its close-block input — the metric and the gate share one source of truth - `inst-isc-tieout`
6. [ ] - `p1` - **RETURN** cleared suspense line; period close unblocked (or an approved exception recorded per PRD) - `inst-isc-return`

## 3. Processes / Business Logic (CDSL)

### Build Direct-Split Entry

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-invoice-line-build`

**Input**: Posted invoice (`invoiceId`, `InvoiceItem[]` with `glCode` / `skuId|planId|priceId` / `pricingSnapshotRef` / revenue-stream / PO / allocation-group / service-period refs), authoritative `TaxBreakdown`, resolved account mappings, `effectiveAt`

**Output**: Balanced line set for `postBalancedEntry(lines, flow=INVOICE_POST, businessKey=invoiceId)`

Builds the S1 **direct-split** entry (PRD § Posting rules S1; Example A). For an invoice with ex-tax total split into recognized-now and deferred portions:

| Line | Side | Account class | Amount |
|------|------|---------------|--------|
| AR | DR | AR | invoice total **incl. tax** |
| Revenue (recognized at post) | CR | Revenue | ex-tax recognized portion |
| Contract liability (deferred) | CR | Contract liability | ex-tax deferred portion |
| Tax | CR | Tax payable | per `TaxBreakdown` |

**Steps**:
1. [ ] - `p1` - Validate amounts tie to `sum(InvoiceItem) + TaxBreakdown` with rounding evidence stored once on the entry - `inst-ib-tie`
2. [ ] - `p1` - **IF** nothing is deferred: the Contract-liability line MUST be **absent** — **no zero-amount placeholder** Contract-liability lines (rejected at validation, AC #4) - `inst-ib-no-placeholder`
3. [ ] - `p1` - Populate item-line traceability: item lines and every Contract-liability line persist `invoice_item_ref`, `sku_or_plan_ref`, `price_id`, `pricing_snapshot_ref`, `po_allocation_group` on `journal_line` itself — Slice 4's `source_invoice_item_ref` and Slice 3's per-item credit-note splits resolve against `journal_line.invoice_item_ref`, so item-level resolution stays unambiguous when one invoice defers several items in the same stream - `inst-ib-item-refs`
4. [ ] - `p1` - Populate a non-null `revenue_stream` on every Revenue and Contract-liability line (AC #26 — the invariant is enforced by the Foundation's commit-trigger / CHECK; this handler populates it); the Contract-liability line carries the stream it will later release into - `inst-ib-stream`
5. [ ] - `p1` - Derive the recognized/deferred split — the split is **derived, not supplied**: upstream supplies item attributes (revenue stream, PO / allocation-group, service-period refs); Slice 4's ScheduleBuilder derives the split via the R1 policy **in the same transaction as the post** — the post fails if derivation fails - `inst-ib-split`
6. [ ] - `p1` - Derivation inputs are a **local snapshot** — the item attributes carried on the materialized post request plus versioned policy / SSP refs resolved from the **same database** — so the in-transaction derivation makes **no network call into Contracts or Catalog**; Foundation C3 (no external blocking dependency on the post path) holds and a Contracts outage cannot take down posting. A stale or missing local snapshot fails the post deterministically rather than blocking on a remote service - `inst-ib-local-snapshot`
7. [ ] - `p1` - Enforce the PO / allocation-group **presence** gate (the trigger-condition and immaterial-exemption matrix is owned by `asc606-recognition`); S1 posts the resulting Contract-liability credit and runs **no** releases. A post blocked by a missing PO / allocation-group tag is counted in `ledger_post_blocked_missing_po_total` and alarms on a sustained rate, so an incomplete Catalog snapshot surfaces as a data-quality signal rather than silently stalling cash collection (softened by 's Catalog default-tagging of ordinary point-in-time lines) - `inst-ib-po-gate`
8. [ ] - `p1` - Insert Tax **as-is** from `TaxBreakdown` (never recomputed — the ledger is not a tax engine, B1): one **separate** Tax-payable (CR) line per `TaxBreakdown` entry, alongside gross AR (incl. tax). Each `TAX_PAYABLE` line carries its own tax dimensions on `journal_line`: `tax_jurisdiction`, `tax_filing_period`, `tax_rate_ref` (rate evidence ref from the breakdown) — the columns and their `CHECK (account_class <> 'TAX_PAYABLE' OR (tax_jurisdiction IS NOT NULL AND tax_filing_period IS NOT NULL))` are Foundation-owned. Carrying the grain on the line is what lets the Foundation's `tax_subbalance` cache be both projected in-transaction and **rebuildable** from `journal_line` like every other cache; Slice 3's per-`(rate, jurisdiction, filing-period)` disaggregation reads `tax_rate_ref` from these same columns - `inst-ib-tax`
9. [ ] - `p1` - Capture the invoice **due date** on the AR `journal_line` from payment terms (due-on-receipt → `due_date = original_posted_at`); the Foundation projects it onto the `ar_invoice_balance` / `ar_payer_balance` grains via `applyBalanceDeltas` - `inst-ib-due-date`
10. [ ] - `p1` - **RETURN** the balanced line set; the handler does **not** open a transaction, claim idempotency, pin the period, project balances, or run the zero-sum check itself — the Foundation does all of that - `inst-ib-return`

**Posting-through-the-Foundation notes (normative)**:

- **Idempotency** at-most-once per `(tenant, INVOICE_POST, invoiceId)` + idempotent replay is the Foundation's `idempotencyClaim` path; a replay with an identical payload returns the prior reference.
- **Period** is pinned via the Foundation's `pinOpenPeriod(tenant, legalEntity)` — `fiscal_period FOR SHARE` + `OPEN` assertion inside the post transaction; `effectiveAt` is subject to the closed-period / material-backdating rules (A6) there.
- **Balance deltas** — the AR (DR), Revenue (CR), Contract-liability (CR), and Tax-payable (CR) deltas — are applied by the Foundation's **`applyBalanceDeltas`** under its canonical lock order, which sorts them into `(table_rank, tenant_id, account_id, currency, payer_tenant_id, invoice_id)` and enforces no-negative on the guarded set. In S1 only AR (debit) and the credit-normal Revenue / Contract-liability / Tax lines are posted, so AR cannot be driven negative by S1 except via an erroneous double reversal — which the reversal flow prevents.
- **Money / rounding** uses the Foundation's `MoneyModule` (banker's rounding, currency-scale registry, residual-cent determinism), so the S1 tax/revenue/contract-liability split rounds identically on every recompute and the commit-time balance check needs no tolerance.
- **Tax-payable sign.** In S1 the handler posts only Tax-payable **credits**, so the negative-balance question (Tax payable MAY go negative during reversal periods, PRD) is **inert** here; the Foundation excludes Tax-payable from the aggregate ≥ 0 check and guards it at the `tax_subbalance` `(jurisdiction, filing-period)` grain instead. The in-transaction enforcement decision for Tax-payable **debits** is deferred to the handler that first posts them (reversals / credit notes).

### Resolve Account Mapping & Suspense Routing

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-invoice-account-mapping`

**Input**: Line candidates with `glCode` / tax-category (Catalog snapshot) and Contract overrides; tenant missing-mapping policy (A5)

**Output**: `(account_class, gl_code)` per line, or suspense-routed line, or `ACCOUNT_MAPPING_MISSING` rejection

**Steps**:
1. [ ] - `p1` - Resolve `(account_class, gl_code)` for each line at post time before the lines are handed to `postBalancedEntry`: **Catalog snapshot class** first, then **Contract override** where applicable (PRD § Account mapping resolution) - `inst-am-resolve`
2. [ ] - `p1` - **IF** mapping is missing and tenant policy = default (route-to-suspense): post the line to the `SUSPENSE` account class with `mapping_status = PENDING` — this opens an exception that **blocks period close** until the line is re-mapped (a compensating reversal + corrected re-post) or an approved exception is recorded (PRD); the Foundation's `TieOutJob` re-derives the open-PENDING set as its close-block input. Never silently maps to a wrong revenue account - `inst-am-suspense`
3. [ ] - `p1` - **IF** mapping is missing and tenant policy = hard-block: reject the post with `ACCOUNT_MAPPING_MISSING` - `inst-am-hardblock`
4. [ ] - `p1` - Emit the suspense-backlog aging metric + alarm on open `mapping_status = PENDING` lines, by tenant: `ledger_suspense_pending_lines_total` (count) and `ledger_suspense_pending_age_seconds` (oldest / histogram); configurable thresholds **warn** above N days and **page** above M days, ticketing Revenue Assurance — because the default is route-to-suspense, PENDING lines otherwise accumulate **silently** during the period and only surface when they block the close at month-end, under deadline - `inst-am-alarm`
5. [ ] - `p1` - **RETURN** resolved mappings / suspense-routed lines - `inst-am-return`

### Suspense Remap (MAPPING_CORRECTION Re-Post)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-suspense-remap`

**Input**: Original entry id, its compensating reversal entry id, corrected mappings

**Output**: Corrected re-posted entry under flow `MAPPING_CORRECTION` (idempotent)

**Steps**:
1. [ ] - `p1` - The clearing workflow is "compensating reversal + corrected re-post", but the original `(tenant, INVOICE_POST, invoiceId)` dedup row **survives the reversal** (and must, per the AC #19 retention rule), so the corrected entry cannot re-use the `INVOICE_POST` flow - `inst-sr-survives`
2. [ ] - `p1` - Use the **additive flow `MAPPING_CORRECTION`**, keyed `(tenant, MAPPING_CORRECTION, invoice_id:correction_id)` - `inst-sr-flow`
3. [ ] - `p1` - `correction_id` MUST be **deterministic** per logical correction — `correction_id = hash(original_entry_id, reversal_entry_id)` — so a retried half rebuilds the **same** dedup key and replays idempotently, never double-posting a second corrected entry under a freshly-generated id - `inst-sr-deterministic`
4. [ ] - `p1` - Set `source_doc_type = MAPPING_CORRECTION` (additive C2 enum); the entry carries source linkage to both the original entry and its reversal - `inst-sr-linkage`
5. [ ] - `p1` - Execute reversal + corrected re-post as one dual-control operator action (preparer/approver, reason code — same governance as material backdating); a retry of either half replays idempotently via the Foundation's idempotency claim - `inst-sr-dualcontrol`
6. [ ] - `p1` - **RETURN** corrected entry reference. Without this flow the suspense path is a dead end: reversible but never correctable - `inst-sr-return`

### AR Aging Rollup (Read-Time)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-ar-aging-rollup`

**Input**: `ar_invoice_balance` cache rows (`balance_minor > 0`), tenant bucket configuration, current time

**Output**: AR aging rollup per payer, bucketed per `(payer, currency)`

S1 is the first AR-affecting fact: a posted invoice debits AR (open AR = posted AR until a payment or note lands). AR aging is exposed as a rollup per payer (PRD Scope "AR balances & aging — HIGH"), `GET /v1/ledger/balances/ar-aging`.

**Steps**:
1. [ ] - `p1` - DB: read `ar_invoice_balance` rows with `balance_minor > 0` for the tenant/payer filter - `inst-ag-read`
2. [ ] - `p1` - Derive buckets **at read time**: **days past due** = now − `due_date`; default buckets **current / 1–30 / 31–60 / 61–90 / 90+ days past due**, tenant-configurable - `inst-ag-buckets`
3. [ ] - `p1` - Aging is by **days past due** vs the invoice **due date** (captured on the AR posting at S1 from payment terms; due-on-receipt → `due_date = original_posted_at`), per PRD *AR aging basis* — **not** days since posting (the prior `original_posted_at` basis is superseded; `original_posted_at` stays only as the due-on-receipt fallback source) - `inst-ag-basis`
4. [ ] - `p1` - Compute buckets per `(payer, currency)` — `ar_invoice_balance` carries `currency`, so amounts of different minor-unit scales are never mixed in one bucket - `inst-ag-currency`
5. [ ] - `p2` - **Debit-note (S4) aging basis**: a debit note posts onto the **same** `ar_invoice_balance` row (per-invoice grain), so by default its delta inherits the originating invoice's `due_date`; when a debit note carries **different** payment terms the debit-note AR `journal_line.due_date` is authoritative and the aging read MUST age that delta from the **line-level** due date — a Slice 4 refinement on the same read - `inst-ag-debit-note`
6. [ ] - `p1` - **RETURN** the rollup. The aging read is a pure read over the existing cache: **no new posted state, no new lock rank** - `inst-ag-return`

### Reversal Line Negation

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-algo-invoice-reversal-negation`

**Input**: Original posted entry (`entryId`, lines), `reason` (payload only)

**Output**: Negated line set for `postBalancedEntry(negatedLines, flow=REVERSAL, businessKey=reverses=entryId)`

The **only** correction shape is **strict line-negation** (AC #23) — the Foundation enforces the append-only / line-negation **mechanism** and the once-per-entry lineage index; this feature owns the invoice-reversal **domain flow**.

**Steps**:
1. [ ] - `p1` - Because `amount_minor` is `CHECK > 0`, AC #23's "same accounts and sides, negated amounts" is realized as **same accounts, flipped side, positive amount** — economically identical line-negation, and explicitly **distinct from gross-replace** (which would additionally re-post a corrected entry) - `inst-rn-shape`
2. [ ] - `p1` - The reversal entry carries `reverses_entry_id` and `reverses_period_id` (the original entry's period), posts at current effective time; the original is untouched - `inst-rn-lineage`
3. [ ] - `p1` - Dedup key `(tenant, REVERSAL, reverses_entry_id)` — `reason` is **payload only** (covered by the Foundation's payload-hash conflict check), never part of the uniqueness key, so two reverse calls with different reasons cannot both post; an entry MUST be reversible **at most once total** - `inst-rn-dedup`
4. [ ] - `p1` - **Amount-correction is out of S1 scope**: S1 offers **only** full strict reversal — there is no in-place restatement of a wrong-**amount** invoice. Re-posting under `INVOICE_POST` is blocked by the surviving dedup row, and `MAPPING_CORRECTION` is for suspense-remap only. Correcting a wrong amount (not a mapping) is therefore **upstream's responsibility** — a reversal plus a **new** `invoiceId` — or a Slice 3 credit/debit note. S1 deliberately does not provide amount-correction - `inst-rn-no-amount-fix`
5. [ ] - `p1` - **RETURN** negated lines for `postBalancedEntry(negatedLines, flow=REVERSAL, businessKey=reverses=entryId)` - `inst-rn-return`

## 4. States (CDSL)

### Invoice Journal Entry State Machine

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-state-invoice-entry`

**States**: POSTED, REVERSED

**Initial State**: POSTED (entries are immutable append-only facts; "REVERSED" is a derived lineage state — a reversal entry points at the original via `reverses_entry_id`; the original row itself is never mutated)

**Transitions**:
1. [ ] - `p1` - **FROM** POSTED **TO** REVERSED **WHEN** a strict line-negation reversal commits (`flow=REVERSAL`, `businessKey=reverses=entryId`); at most **once total** per entry (partial unique index on `(tenant_id, reverses_period_id, reverses_entry_id) WHERE reverses_entry_id IS NOT NULL`) - `inst-st-entry-reverse`
2. [ ] - `p1` - **FROM** REVERSED **TO** (any) — **forbidden**: no reverse-of-a-reversal (`CANNOT_REVERSE_REVERSAL`, 422) and no second reversal of the original; correction continues upstream with a new `invoiceId` or a Slice 3 note - `inst-st-entry-terminal`

### Suspense Line State Machine

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-state-suspense-line`

**States**: PENDING, CLEARED, APPROVED_EXCEPTION

**Initial State**: PENDING (line posted to `SUSPENSE` account class with `mapping_status = PENDING` on missing mapping, A5)

**Transitions**:
1. [ ] - `p1` - **FROM** PENDING **TO** CLEARED **WHEN** the dual-control clearing completes: compensating reversal + corrected re-post under `MAPPING_CORRECTION` (`cpt-cf-bss-ledger-algo-suspense-remap`) - `inst-st-susp-cleared`
2. [ ] - `p1` - **FROM** PENDING **TO** APPROVED_EXCEPTION **WHEN** an approved exception is recorded (PRD) — lifts the period-close block without re-post - `inst-st-susp-exception`
3. [ ] - `p1` - **WHILE** any line is PENDING: period close is **blocked** (the Foundation's `TieOutJob` re-derives the open-PENDING set as its close-block input) - `inst-st-susp-close-block`

## 5. Definitions of Done

### Direct-Split Invoice Posting

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-invoice-post`

The system **MUST** post a balanced S1 direct-split entry (DR AR gross incl. tax / CR Revenue / CR Contract-liability / CR Tax payable per `TaxBreakdown`) through the Foundation's `postBalancedEntry(lines, flow=INVOICE_POST, businessKey=invoiceId)`, with amounts tied to `sum(InvoiceItem) + TaxBreakdown`, rounding evidence stored once on the entry, no zero-amount Contract-liability placeholders, item-traceability refs and a non-null `revenue_stream` on every Revenue / Contract-liability line, tax dimensions on every `TAX_PAYABLE` line, `due_date` on the AR line, and the in-transaction local-snapshot recognized/deferred split derivation.

**Implements**:
- `cpt-cf-bss-ledger-flow-invoice-post`
- `cpt-cf-bss-ledger-algo-invoice-line-build`

**Touches**:
- API: `POST /v1/ledger/journal-entries`
- DB: `journal_entry`, `journal_line`, balance caches, `idempotency_dedup` (all Foundation-owned; via data-access API only)
- Entities: `JournalEntry`, `JournalLine`, `TaxBreakdown`, `InvoiceItem`

### Account Mapping & Suspense Routing

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-invoice-suspense-routing`

The system **MUST** resolve `(account_class, gl_code)` per line (Catalog snapshot first, then Contract override), route missing-mapping lines to `SUSPENSE` with `mapping_status = PENDING` (blocking period close) or hard-block with `ACCOUNT_MAPPING_MISSING` per tenant policy, emit the suspense-backlog aging metrics/alarms, and support the idempotent `MAPPING_CORRECTION` clearing flow with deterministic `correction_id`.

**Implements**:
- `cpt-cf-bss-ledger-flow-invoice-suspense-clearing`
- `cpt-cf-bss-ledger-algo-invoice-account-mapping`
- `cpt-cf-bss-ledger-algo-suspense-remap`
- `cpt-cf-bss-ledger-state-suspense-line`

**Touches**:
- API: `POST /v1/ledger/journal-entries`
- DB: `journal_line` (`mapping_status`), `idempotency_dedup` (via Foundation)
- Entities: `AccountMappingResolver`, `SuspenseLine`

### Strict Line-Negation Reversal

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-invoice-reversal`

The system **MUST** reverse a posted invoice entry via strict line-negation only (same accounts, flipped side, positive amount), at most once total per entry, with `reason` as payload only, rejecting reverse-of-a-reversal (`CANNOT_REVERSE_REVERSAL`) and providing no in-place amount correction, validating original-entry existence at post time.

**Implements**:
- `cpt-cf-bss-ledger-flow-invoice-reverse`
- `cpt-cf-bss-ledger-algo-invoice-reversal-negation`
- `cpt-cf-bss-ledger-state-invoice-entry`

**Touches**:
- API: `POST /v1/ledger/journal-entries/{entryId}/reversals`
- DB: `journal_entry` (`reverses_entry_id`, `reverses_period_id`), via Foundation
- Entities: `JournalEntry`

### AR Balances & Aging Read

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-invoice-ar-aging`

The system **MUST** serve current-cache balance reads and a read-time AR aging rollup per payer bucketed per `(payer, currency)` by days past due vs `due_date` (default current/1–30/31–60/61–90/90+, tenant-configurable), with no new posted state and no new lock rank; as-of balances are out of S1 scope.

**Implements**:
- `cpt-cf-bss-ledger-flow-invoice-balance-inquiry`
- `cpt-cf-bss-ledger-algo-ar-aging-rollup`

**Touches**:
- API: `GET /v1/ledger/balances`, `GET /v1/ledger/balances/ar-aging`
- DB: `ar_invoice_balance`, `ar_payer_balance` (Foundation-owned caches, read-only)
- Entities: `ArAgingBucket`

### Invoice-Posting API Surface

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-dod-invoice-api`

The system **MUST** expose the six §7.1 endpoints per the `rest-api-design` standard behind the inbound API gateway (JSON, OAuth 2.0, tenant from authenticated context + RLS, `/v1` versioning, minor-unit money objects, never floats), with 201-first-post / 200-idempotent-replay semantics and RFC 9457 problem responses passing Foundation invariant codes through unchanged plus the three domain codes.

**Implements**:
- `cpt-cf-bss-ledger-flow-invoice-post`
- `cpt-cf-bss-ledger-flow-invoice-reverse`
- `cpt-cf-bss-ledger-flow-invoice-balance-inquiry`

**Touches**:
- API: `POST /v1/ledger/journal-entries`, `POST /v1/ledger/journal-entries/{entryId}/reversals`, `GET /v1/ledger/balances`, `GET /v1/ledger/balances/ar-aging`, `GET /v1/ledger/journal-entries/{entryId}`, `GET /v1/ledger/journal-lines`
- DB: none directly (all via Foundation data-access API)
- Entities: RFC 9457 problem model

## 6. Acceptance Criteria

Derived from the source design's testing architecture (§7.8). Mocking by level: Unit = in-memory fakes (including a fake Foundation API); Integration = real PostgreSQL via testcontainers behind the real Foundation; API = real DB + in-process HTTP, mock only the AuthZ enforcer + true external clients; E2E = nothing mocked.

- [ ] **Unit:** S1 line builder — direct split; deferred-zero ⇒ no contract-liability line; gross AR incl. tax; Revenue/Contract-liability line always carries a stream; reversal side-flip with positive amount; `AccountMappingResolver` (Catalog-first then Contract override; missing ⇒ route-to-suspense with `mapping_status = PENDING` vs hard-block per tenant policy); `TaxBreakdown` accepted as-is (never recomputed) with tax dimensions populated on each `TAX_PAYABLE` line; AR aging bucket derivation (days-past-due vs `due_date`, per `(payer, currency)`); the builder calls `postBalancedEntry(flow=INVOICE_POST, businessKey=invoiceId)` with balanced lines (fake Foundation)
- [ ] **Integration (testcontainers Postgres, real Foundation):** closed-payer invoice post rejected (`PAYER_CLOSED`) while a reversal on the same payer still posts; a Revenue/Contract-liability line missing `revenue_stream` is rejected by the Foundation invariant; a suspense line blocks period close until cleared (the open-PENDING set re-derived by the tie-out); the `MAPPING_CORRECTION` corrected re-post replays idempotently on a retried half (deterministic `correction_id`); reverse-of-a-reversal rejected (`CANNOT_REVERSE_REVERSAL`); an entry is reversible at most once total even across two reverse calls with different reasons; amount-correction has no in-place path (re-post under `INVOICE_POST` blocked by the surviving dedup row)
- [ ] **API:** RFC 9457 mapping for the domain codes (`PAYER_CLOSED`, `CANNOT_REVERSE_REVERSAL`, `ACCOUNT_MAPPING_MISSING`) and pass-through of the Foundation invariant codes; replay (identical payload ⇒ `200` prior reference), conflict (different payload ⇒ `409`); `POST /journal-entries`, `POST /{entryId}/reversals`, `GET /balances`, `GET /balances/ar-aging`, `GET /journal-entries/{entryId}`, `GET /journal-lines` cursor pagination; audit-retrieval response carries who/when/source/correlation (AC #8)
- [ ] **E2E:** post invoice → read balance → read AR aging → reverse → tie-out, end-to-end with no mocks (through the real Foundation)
- [ ] **Concurrency (domain-specific):** concurrent posts of the same `invoiceId` ⇒ exactly one ledger effect; loser blocks then returns the prior reference (winner commits) or posts (winner aborts) — never an absent reference; two concurrent reversals of the same entry with different reasons ⇒ exactly one reversal, the other replays/409 (cross-table deadlock-freedom and first-touch upsert-race tests are Foundation-owned)

## 7. Additional Context

### 7.1 REST API Surface (feature-owned endpoints)

**Conventions.** REST per the `rest-api-design` standard, behind the inbound API gateway. JSON; OAuth 2.0; tenant from the authenticated context (also enforced by RLS, §7.5). Versioned under `/v1`. All money fields are `{ "amountMinor": <integer>, "currency": "USD", "scale": 2 }` — never floats. Mutating posts are idempotent on a client-supplied business key (`invoiceId`). This is the **handler-facing HTTP surface** for invoice-posting; it funnels into the Foundation's data-access API (the post/reverse/read HTTP surface is handler-owned). The seller-provisioning endpoint is **not** here — it belongs to the Foundation.

| Method | Path | Purpose | Idempotency |
|--------|------|---------|-------------|
| `POST` | `/v1/ledger/journal-entries` | Post an S1 invoice entry (the only post type in slice 1). Body: `invoiceId`, lines (or invoice items + `taxBreakdown` + revenue-stream), `effectiveAt`, mapping refs. The handler builds balanced lines and calls `postBalancedEntry(lines, flow=INVOICE_POST, businessKey=invoiceId)`. sub-resource create, was `:post-invoice`. | At-most-once per `(tenant, INVOICE_POST, invoiceId)` (Foundation `idempotencyClaim`). |
| `POST` | `/v1/ledger/journal-entries/{entryId}/reversals` | Post a strict line-negation reversal; calls `postBalancedEntry(negatedLines, flow=REVERSAL, businessKey=reverses=entryId)`. Body: `reason` (payload only). was `{entryId}:reverse` — colon custom methods don't route on axum 0.8 / matchit 0.8.4 (dynamic suffix unsupported). | At-most-once per `(tenant, REVERSAL, reverses=entryId)` — an entry is reversible **once total**. |
| `GET` | `/v1/ledger/balances` | Read balances (filter: `accountClass`, `currency`, `payerTenantId`, optional `invoiceId` for the AR-invoice grain). Warm read from the Foundation's balance cache. | Warm read from the balance cache. |
| `GET` | `/v1/ledger/journal-entries/{entryId}` | Retrieve a posted entry with its lines + source linkage + actor/correlation (AC #8). | — |
| `GET` | `/v1/ledger/journal-lines` | Paginated transaction history (cursor pagination; filter: payer, account class, period, source business id). | — |
| `GET` | `/v1/ledger/balances/ar-aging` | AR aging rollup per payer. Buckets derived **at read time** from `ar_invoice_balance` (`balance_minor > 0`, **days past due** = now − `due_date`); default buckets **current / 1–30 / 31–60 / 61–90 / 90+ days past due**, tenant-configurable; computed per `(payer, currency)`. A pure read over the existing cache — no new posted state, no new lock rank. | Warm read from the balance cache. |

**Success / replay semantics (not errors):** a first successful post returns `201` with the posting reference. An idempotent replay (same key + identical payload) returns `200` with the **prior** posting reference (`entryId`, `postedAtUtc`, `status`), not an error body — resolved by the Foundation's `idempotencyClaim`.

**As-of balance.** All balance reads return the **current** cached value. A point-in-time / as-of balance is **out of S1 scope**; when a reporting / reconciliation slice needs it, it is reconstructed from `journal_line` (the caller states the basis — `posted_at_utc` vs `effective_at`), never derived from the current cache.

### 7.2 Problem Responses (RFC 9457)

Error responses use `application/problem+json` (RFC 9457): `{ type, title, status, detail, instance, code, tenantId? }`. The universal-invariant codes (`LEDGER_ENTRY_UNBALANCED`, `LEDGER_ENTRY_EMPTY`, `MIXED_PAYER_TENANT`, `MISSING_PAYER`, `MIXED_LEGAL_ENTITY`, `IDEMPOTENCY_PAYLOAD_CONFLICT`, `NEGATIVE_BALANCE_VIOLATION`, `PERIOD_CLOSED`, `ACCOUNT_CLOSED`, `CURRENCY_SCALE_LOCKED`, `CLOCK_SKEW_QUARANTINE`, `TENANT_POSTING_LOCKED`, `LEDGER_ENTRY_TOO_LARGE`, `AMOUNT_OUT_OF_RANGE`) are raised by the **Foundation** on `postBalancedEntry` and surfaced unchanged through this endpoint. The codes below are **invoice-post-domain-specific** — raised by this feature:

| `code` | HTTP | Trigger |
|--------|------|---------|
| `PAYER_CLOSED` | 422 | Invoice post against a payer with `payer_state.lifecycle_state = CLOSED` (AC #21); the Foundation asserts the payer-OPEN gate for payer-posting handlers, this handler is one. |
| `CANNOT_REVERSE_REVERSAL` | 422 | Reverse requested on an entry with `source_doc_type = REVERSAL` — no undo-the-undo chains. |
| `ACCOUNT_MAPPING_MISSING` | 422 | No Catalog/Contract mapping and tenant policy = hard-block (A5). |

### 7.3 Events Surface

The invoice-posting feature does **not** own an events surface of its own — success events and invariant alarms are emitted by the **Foundation** as a consequence of the posts this feature drives:

- `billing.ledger.entry.posted` — emitted via the Foundation's transactional **outbox** after an invoice entry commits (`entryId`, `tenantId`, `sourceDocType`, `sourceBusinessId`, `postedAtUtc`, line summary).
- `billing.ledger.entry.reversed` — emitted via the outbox after a reversal commits (`entryId`, `reversesEntryId`, `reason`).
- `billing.ledger.invariant.alarm` — on zero-sum / negative-balance / idempotency-collision / clock-skew, on the Foundation's **separate committed transaction** so the alarm survives an aborting post.

Payloads carry **internal identifiers only** (no PII). Relay ordering (per-tenant FIFO by `created_seq`) and the consumer registry are Foundation-owned. This feature adds **no** new event kinds and **no** new consumer-registry rows.

### 7.4 Data Model (feature-owned tables)

This feature defines **no** reference tables and **no** shared-table DDL — every table it reads or writes (the `journal_entry` / `journal_line` truth, the balance caches, `idempotency_dedup`, `tenant_account`, `fiscal_period`, `currency_scale_registry`, `tenant_posting_lock`, `payer_state`, `event_outbox`) is owned and DDL-defined by the **Foundation** (see 01-repository-foundation.md §Component Model), and seeded by the Foundation's seller-provisioning endpoint. The feature **asserts** these exist at post time (account `OPEN`, period `OPEN`, currency scale, payer OPEN) via the Foundation's gates; it never creates them.

Invoice-post-specific reference data: **none** beyond what the Foundation already owns. The invoice-post domain consumes its mapping inputs (Catalog `glCode` / tax-category snapshot, Contract PO / allocation-group ids, the `TaxBreakdown`) as **per-request facts** delivered on the post call, not as ledger-owned reference rows.

### 7.5 Security & AuthZ

- **Tenant isolation (C1).** RLS on all (Foundation-owned) tables; the app sets `app.tenant_id` per request. The **no-mixed-payer-tenant** invariant is enforced at the journal level by the Foundation's commit trigger (mapped to `MIXED_PAYER_TENANT`); **no-mixed-legal-entity** is structural — `legal_entity_id` lives on `journal_entry` only — so an invoice entry can never straddle tenants or legal entities.
- **Tenant-hierarchy payment delegation — payer-resolution rule (in scope for this feature's contract).** `payer_tenant_id` is the **nearest `self_managed` ancestor-or-self** of `resource_tenant` (`self_managed` = billing boundary — a managed child consolidates up, a self-managed tenant is its own payer and never consolidates upward); AR posts **directly** to the resolved payer; `resource_tenant` is attribution metadata. **Resolution runs upstream** (Rating/Invoice) over the platform tenant tree + `self_managed`; the handler **records** the resolved `payer_tenant_id` on the lines and relies on the Foundation to enforce the structural invariants (payer present, no-mixed-payer-tenant) — neither the handler nor the Foundation walks the tenant tree on the write path. **(Rev — PRD contract, ledger-side; pending Rating/Invoice confirmation)** The posting call carries, beyond `payer_tenant_id`: `resource_tenant_id` and a **resolution-provenance stamp** (`resolved_as_of` + `tenant_tree_version_ref`), stored as line audit metadata so the resolution is reproducible. Resolution is pinned **as-of the originating business event** and is **never** re-resolved later. A call with no resolvable `payer_tenant_id` is **rejected** `MISSING_PAYER` (422) — never defaulted. **Tenant-tree mutation:** a later `self_managed` flip / re-parent **does not** retroactively change already-posted AR (posted facts immutable); future postings use the payer upstream resolves at their own event time; the ledger does **no** automatic re-attribution. **Deferred (post-MVP):** a ledger-side guard re-validating the resolved payer against the tree (Variant C); inter-tenant settlement transfer / payout / `PARENT_SUMMARY` summary invoice. (PRD § Multi-tenant; AC #27.)
- **Audit retrieval (AC #8).** "Who/when/source/correlation" is retrievable per entry via the Foundation's `journal_entry` columns (`posted_by_actor_id`, `origin`, `posted_at_utc`, `source_doc_type` / `source_business_id` / `reverses_entry_id`, `correlation_id`); human-readable PII is **not** stored here.
- **AuthZ.** Posting and reversal require billing-poster / approver scopes; reversal and material-backdating follow dual-control per policy (detailed RBAC in implementation).

### 7.6 Feature Metrics

Prometheus scrape metrics specific to the invoice-post domain; the engine-level metrics (post duration, balance-read duration, bill-run throughput, negative-balance / unbalanced / idempotency counters, tie-out variance, lock-wait) are Foundation-owned.

| Metric (`ledger_*`) | Vector | Target threshold | Purpose |
|---------------------|--------|------------------|---------|
| `ledger_suspense_pending_lines_total` / `ledger_suspense_pending_age_seconds` (by tenant) | Reliability | warn > N days / page > M days | Suspense-backlog volume + age — surface un-mapped lines before they block close. |
| `ledger_post_blocked_missing_po_total` (by tenant) | Reliability | alarm on sustained rate | Posts blocked by a missing PO / allocation-group tag — surfaces Catalog tagging gaps before they stall cash collection. |

### 7.7 NFR Mapping

The invoice-post path inherits the Foundation's v1 SLOs (read p95 ≤ 200 ms, write p95 ≤ 500 ms, ≥ 2,000 invoices/min, ≤ 60 min/100k), since every invoice post and balance read funnels through the Foundation's data-access API. This feature introduces no separate NFR; its contribution to the hot-row risk (the `ar_payer_balance` payer-aggregate row credited on every invoice) is analysed in §7.9 and load-tested as part of the Foundation's B3 gate.

### 7.8 Testing Architecture

Correctness is the value of the ledger; per `design-standards.md`, the testing section is concrete. These are the **invoice-post domain** tests; the engine-level correctness tests (append-only enforcement, the deferrable leaf-partition constraint trigger, conditional no-negative CHECK, RLS, idempotency PK, deadlock-free lock order, first-touch upsert race, tie-out recompute) are Foundation-owned. Mocking by level: Unit = in-memory fakes (including a fake Foundation API); Integration = real PostgreSQL via testcontainers behind the real Foundation; API = real DB + in-process HTTP, mock only the AuthZ enforcer + true external clients; E2E = nothing mocked. The concrete test matrix is captured as the checklist in [6. Acceptance Criteria](#6-acceptance-criteria) (Levels 1–4 + concurrency).

### 7.9 Risks, Open Questions & Deferred Work

**Open decisions needing PM / Finance** specific to the invoice-post domain (defaults applied; see §1.6):

| Topic | Default | Needs |
|-------|---------|-------|
| Missing-mapping policy | route-to-suspense + close-block, tenant-configurable | Finance default + per-tenant override? |
| Backdating threshold | 5 business days, range [1..30] | Confirm default + whether tenants override (evaluated by the Foundation's `FiscalPeriodGuard`) |
| AR aging buckets | read-time derivation from `ar_invoice_balance.due_date` (days past due); default current/1–30/31–60/61–90/90+; tenant-configurable | Finance (bucket config) |

The money-type / NFR / tie-out / hot-row / bill-run-scale decisions are Foundation-level.

**Known risks (domain).** (1) **Hot-row contention on `ar_payer_balance`** — a single-tenant bill run credits the payer-aggregate AR row on every invoice; the upsert row lock serializes these within a tenant. In S1 this is the only active non-shardable hot row (hard no-negative at the payer-aggregate grain); its ceiling MUST be load-tested before commit as part of the Foundation's B3 gate (Mode S enabled). The mitigation / fallback (per-worker partial sums merged on a short interval, with the un-sharded `ar_invoice_balance` per-invoice CHECK as the interim guard) is Foundation-owned. (2) **Suspense backlog stalling close** — mitigated by the aging metric + alarm surfacing un-mapped lines on the first affected invoice. (3) **PO / allocation-group tagging gaps stalling cash** — mitigated by the `ledger_post_blocked_missing_po_total` alarm.

**Deferred work.** Amount-correction in place (— out of scope, use reversal + new `invoiceId` or a Slice 3 note); the sibling feature slices listed in §1.5. The seams the invoice-post domain relies on (account classes, reversal shape, FX-ready money fields, success-event outbox) are provided by the Foundation, so the sibling features attach without reshaping the posted journal.

### 7.10 Decision Log (Needs Discussion)

Consolidated decision log for the invoice-post-domain blocker items. **None block this feature spec** — all are recorded as assumptions in §1.6 / §7.9. The engine-level decisions (B2/B3/B4/B5/B11–B17 — NFR, hot-row scale, tie-out window, money type, partition/trigger hardening) live in the Foundation.

| Item | Decision | Status | Owner |
|------|----------|--------|-------|
| Tax presentation: net vs gross | Ledger posts **gross** AR + a separate Tax-payable line; net/tax-split is export/inquiry presentation only | ✅ Resolved | Product |
| Missing account-mapping policy | route-to-suspense + period-close block; tenant-configurable to hard-block | ✅ Accepted default | — |
| Material-backdating threshold | 5 business days, range [1..30], tenant-overridable (A6; evaluated by the Foundation's `FiscalPeriodGuard`) | ✅ Accepted default | — |
| Suspense remap re-post flow | additive flow `MAPPING_CORRECTION` keyed `(tenant, MAPPING_CORRECTION, invoice_id:correction_id)` | ✅ Proposed default | — |
| AR aging buckets | read-time derivation from `ar_invoice_balance.due_date` (**days past due**, — supersedes the posting-date basis); default current/1–30/31–60/61–90/90+; tenant-configurable | ✅ Proposed default | Finance (bucket config) |
| PO / allocation-group presence gate | Gate stays; residual is **Catalog data quality** (softened by); monitored via `ledger_post_blocked_missing_po_total` + alarm so Catalog gaps surface before stalling cash | ✅ Accepted 2026-06-16 (data-quality risk) | Catalog (data) |

### 7.11 References

- **Repository-foundation** (see 01-repository-foundation.md §Component Model) — the shared posting engine this feature posts through: schema/DDL, universal invariants, total lock order, data-access API (`postBalancedEntry` / `applyBalanceDeltas` / `idempotencyClaim` / `pinOpenPeriod` / `applyCounterDelta`; read-then-write ordering via `SERIALIZABLE`/SSI, not a locked-read op), and seller-provisioning endpoint.
- [PRD.md](../PRD.md) — upstream PRD (posting rules S1–S6, account classes, invariants, ACs, reconciliation, NFRs).
- Parent module scope (AR, ledger, financial posting) and program billing architecture — Billing Module / Billing System PRDs (legacy refs preserved in the source design).
- Contracts & Agreements, Metering & Pricing, Product Catalog & Marketplace, Subscriptions & Entitlements PRDs — interface sources (PO tags, billable items, glCode/tax category, recurring charges).
- Inbound API gateway ADR — API exposure pattern (C4).
- Azure billing domain model design — related BSS domain model (this design diverges to integer minor-unit money per AC #16).
