---
refs:
  - bss/manifest/vz-arch-manifest-bss-only.md
  - bss/prd/PRD-billing-module-202601120119/PRD-billing-module-202601120119.md
  - bss/prd/PRD-billing-system-202601120119/PRD-billing-system-202601120119.md
  - bss/prd/PRD-contracts-agreements-202601120119/PRD-contracts-agreements-202601120119.md
  - bss/prd/PRD-metering-pricing-module-202601120119/PRD-metering-pricing-module-202601120119.md
  - bss/prd/PRD-product-catalog-marketplace-202601120119/PRD-product-catalog-marketplace-202601120119.md
  - bss/prd/PRD-subscriptions-entitlements-202601120119/PRD-subscriptions-entitlements-202601120119.md
---

Created:  2026-06-11 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- migration-note: migrated from legacy Virtuozzo PRD format to virtuozzo-sdlc kit layout (cpt-* sub-IDs, 17-section outline). Original preserved unchanged at docs/bss/prd/PRD-billing-ledger-balances-202604041200/. Confluence metadata preserved below. -->
<!-- CONFLUENCE_TITLE: [BSS]: Billing Ledger & Balances — Double-Entry, AR, ASC 606-Compatible Posting -->
<!-- Related: bss/prd/PRD-billing-ledger-balances-202604041200 | Upstream: Rating, Subscriptions, Catalog, Contracts | Downstream: ERP/GL, Payments -->

# PRD — Billing Ledger & Balances

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Architecture Alignment](#2-architecture-alignment)
  - [2.1 PRD vs Design Ownership](#21-prd-vs-design-ownership)
  - [2.2 Manifest & Repository Traceability (Proof Matrix)](#22-manifest--repository-traceability-proof-matrix)
- [3. Actors](#3-actors)
  - [3.1 Human Actors](#31-human-actors)
  - [3.2 System Actors](#32-system-actors)
- [4. Operational Concept & Environment](#4-operational-concept--environment)
  - [4.1 Module-Specific Environment Constraints](#41-module-specific-environment-constraints)
- [5. Scope](#5-scope)
  - [5.1 In Scope](#51-in-scope)
  - [5.2 Out of Scope](#52-out-of-scope)
- [6. Functional Requirements](#6-functional-requirements)
  - [6.1 Double-Entry Journal Engine](#61-double-entry-journal-engine)
  - [6.2 Posting Rules — Invoice, Payment, Adjustments, Refund (S1–S5)](#62-posting-rules--invoice-payment-adjustments-refund-s1s5)
  - [6.3 ASC 606 Revenue Recognition](#63-asc-606-revenue-recognition)
  - [6.4 Immutability, Audit & Compliance](#64-immutability-audit--compliance)
  - [6.5 Multi-Tenant & Multi-Axis Posting](#65-multi-tenant--multi-axis-posting)
  - [6.6 Idempotency & Replay](#66-idempotency--replay)
  - [6.7 Reconciliation & Period Close](#67-reconciliation--period-close)
  - [6.8 Money, Rounding & Foreign Exchange](#68-money-rounding--foreign-exchange)
  - [6.9 Chargebacks & Disputes](#69-chargebacks--disputes)
  - [6.10 Lifecycle, Ordering & Governance](#610-lifecycle-ordering--governance)
- [7. Non-Functional Requirements](#7-non-functional-requirements)
  - [7.1 NFR Inclusions](#71-nfr-inclusions)
  - [7.2 NFR Exclusions](#72-nfr-exclusions)
- [8. Five Quality Vectors Analysis](#8-five-quality-vectors-analysis)
- [9. Public Library Interfaces](#9-public-library-interfaces)
  - [9.1 Public API Surface](#91-public-api-surface)
  - [9.2 External Integration Contracts](#92-external-integration-contracts)
- [10. Use Cases](#10-use-cases)
- [11. User Interaction and Design](#11-user-interaction-and-design)
- [12. Acceptance Criteria](#12-acceptance-criteria)
  - [Double-entry](#double-entry)
  - [Invoice and adjustments](#invoice-and-adjustments)
  - [Payments](#payments)
  - [Reconciliation](#reconciliation)
  - [Audit](#audit)
  - [Revenue recognition](#revenue-recognition)
  - [High-risk and cross-cutting](#high-risk-and-cross-cutting)
  - [Non-Functional Requirements (Show-Stoppers)](#non-functional-requirements-show-stoppers)
- [13. Dependencies](#13-dependencies)
- [14. Assumptions](#14-assumptions)
- [15. Open Questions](#15-open-questions)
- [16. Risks](#16-risks)
- [17. Reference Materials](#17-reference-materials)
  - [17.1 Worked Examples (illustrative)](#171-worked-examples-illustrative)
  - [17.2 Textual Diagrams](#172-textual-diagrams)
  - [17.3 Test Strategy (verification obligations)](#173-test-strategy-verification-obligations)
  - [17.4 Observability — Invariant Alarms](#174-observability--invariant-alarms)

<!-- /toc -->

<!-- migration-note: kit virtuozzo-sdlc defines no document-level `prd` ID kind; document identity is the file path + registry entry. Sub-IDs (fr/nfr/actor/usecase/interface/contract) carry traceability. -->

## 1. Overview

### 1.1 Purpose

The **Billing Ledger** is an **append-only, double-entry** subledger within BSS that records every financially material movement from **billable artifacts → invoice post → settlement** with **balanced** journal lines, **multi-tenant** isolation, and **immutable** audit history. It maintains **AR balances**, **aging**, and **statement** inputs consistent with manifest §4.4 invariants, and supports **ASC 606**-compatible **revenue recognition** for invoice-originated contract liabilities with idempotent **export** to **ERP/GL**.

It interlocks cleanly with **CreditNote/DebitNote**, **PaymentAllocation**, and **refunds** without mutating posted invoice line financials — corrections flow through compensating entries only.

### 1.2 Background / Problem Statement

BSS requires a high-integrity financial core: posted invoices must be immutable, AR and revenue must be auditable, and exports to corporate ERP/GL must be idempotent and replay-safe (manifest §1.2.1, §4.4). Without a normative double-entry ledger, posting semantics (deferral, refunds, credit notes, recognition, multi-currency) risk being decided ad hoc in Design, producing non-auditable, ASC-non-compliant financials.

This PRD fixes posting **meaning**, **scope** boundaries, and **policy** expectations so Design implements — not invents — unsettled business rules.

<!-- migration-note: legacy "Industry alignment (optional)" notes folded here as background. -->
Industry alignment: an append-only journal with compensating entries matches audit/SOX expectations for high-integrity billing cores; treating BSS as operational subledger and ERP as GL of record with idempotent export and reconciliation variance is a standard enterprise pattern (manifest §4.4 exports).

### 1.3 Goals (Business Outcomes)

> Targets below are measurable business outcomes. Numeric thresholds marked *(commit)* are owned by PM Team and tracked in §15 Open Questions / §7.1; until committed they carry the draft target shown.

- **Posting integrity**: 100% of posted journal entries are balanced per tenant (zero zero-sum violations reaching production) — verified continuously via the §6.8 invariant alarm.
- **AR auditability**: AR ledger ties out to the derived AR projection within reconciliation tolerance (draft: ≤ 1 minor unit per 1,000 posted lines, §6.7) on the daily reconciliation job; variance above tolerance blocks period close.
- **ASC 606 readiness**: 100% of deferred invoice lines carry PO/allocation-group lineage resolvable to a recognition schedule (no recognition journal without invoice-item linkage).
- **Export reliability**: idempotent, replay-safe ERP/GL export with a journal-post → ERP-ack SLA *(commit; draft target tracked in §7.1)* and zero dropped posted facts on export failure.
- **Recovery**: meet RTO ≤ 60 min / RPO ≤ 5 min per region for the posting path (§7.1).

### 1.4 Glossary

| **Term** | **Definition** |
|----------|----------------|
| **Journal entry** | A balanced set of debit/credit lines posted in one atomic **ledger transaction** with a single posting identifier, timestamp, and reason. |
| **Ledger book** | Named partition of accounts (e.g., **AR subledger**, **revenue recognition**, **cash clearing**) scoped by **legal entity** / **account** / **tenant** per Design. |
| **Posted invoice** | Invoice with `status=posted` per §4.4; financially **immutable**; subsequent changes via **CreditNote/DebitNote** only. |
| **Contract liability (primary term)** | ASC 606: obligation to transfer goods/services, recorded as a **credit** to the deferral / liability account class. **Use *contract liability* in this PRD's normative text.** |
| **Deferred revenue (synonym only)** | Informal label for the **same** ledger account class and postings as **contract liability**. Do not treat "deferred revenue" as a different product mechanism—one economic concept, one primary name (**contract liability**). |
| **Recognition schedule** | Time- or event-based release of contract liability to **revenue** for multi-period subscriptions/usage minimums. |
| **Billing Ledger** (this PRD) | Posted double-entry financial subledger in BSS (AR, revenue, contract liability, posted tax, cash vs AR/clearing). Distinct from **Rating** operational charge trace upstream of invoice post. |
| **Rating ledger** | Operational metering/pricing trace (e.g. rated charges); upstream of Billing Ledger until invoice post. |
| **Unallocated cash (Pattern A / suspense pool)** | Settled funds held in a **suspense / unapplied** bucket before allocation to **AR** and before any **business election** to treat the balance as **reusable customer credit**. This is *cash location only*—not yet an intentional on-account "credit for reuse." |
| **Reusable customer credit (on-account)** | Balance the customer (or policy) has explicitly made available to apply to **future** invoices, wallet-style holds, or similar—**not** only because cash arrived before matching. Journals MUST be traceable to a named business action. **Design** may use one or two ledger buckets; the **product** distinction (unallocated vs on-account credit) is **not** optional. |
| **Overpayment** | Payment in excess of open AR applied in the same settlement; the residual is first **unallocated** (Pattern A), then handled as refund, future allocation, or on-account **credit election** per S2 and policy. |

**One term per idea (normative phrasing in Design must follow this split):**

| **Idea** | **Use this term in PRD / product language** | **Do not** |
|----------|---------------------------------------------|------------|
| ASC deferral / obligation bucket | **Contract liability** (primary); "deferred revenue" = synonym in prose only, same account class. | Do not use two different accounting **meanings** for the two English phrases. |
| Inbound not yet on specific invoices | **Unallocated cash** / unapplied **pool** until allocated. | Do not call all such balances "customer credit." |
| Durably on-account, reusable, or wallet | **Reusable customer credit** (on-account), only when policy/user action **elects** reuse. | — |

**Section shorthand**: **S1**–**S6** = the six posting rules in §6.2/§6.3 (Invoice post, Payment, Credit note, Debit note, Refund, Recognition schedule); **S7** = ASC 606 compliance (§6.3); **S8** = cross-cutting requirements (§6.7/§6.8/§6.10).

## 2. Architecture Alignment

| **Field** | **Value** |
|-----------|----------|
| **Applicable Manifest(s)** | BSS |
| **Relevant Chapters** | §1.2.1 (double-entry ledger, AR, aging, dunning); §2.1.4 (deterministic monetization, immutable posted financials); §4.4 Billing and Invoicing; §4.5 Payments, Refunds, and Credits; §8 data model (PaymentAllocation); §9 Security, Compliance, and Audit |

> **Scope relationship**: This PRD **specializes** the **Billing Ledger & Balances / double-entry** slice referenced in `PRD-billing-module-202601120119` (P0 scope item). It adds normative posting rules, chart-level account classes, journal entry patterns, ASC 606 alignment requirements, and reconciliation flows. **API shapes, CoA codes, and storage DDL** remain in **Design**. **Rating** produces `BillableItem`; **Billing** owns posted financial truth and **AR**; **Payments** owns PSP mechanics—per manifest boundaries.

**Manifest fit**: §4.4 "Turn billable items … into … invoices with posting immutability"; §1.2.1 "double-entry ledger, balances, aging, and dunning".

<!-- migration-note: legacy "PRD scope vs Design" preserved here as architecture-alignment ownership boundary. -->

### 2.1 PRD vs Design Ownership

| **Layer** | **Owns in this PRD** | **Defers to Design (implementation only)** |
|----------|----------------------|---------------------------------------------|
| **Accounting / control intent** | Double-entry invariants, posting *patterns* (unapplied vs AR vs refund), ASC alignment *rules*, idempotency *by flow*, SoR *relationship* to manifest, what is **in / out of scope** | — |
| **Parameters** | Numeric tolerances, rate tables, specific GL codes, idempotency key *fields*, exact RBAC role names, suspense account numbers, ERP connector retry backoff | Refined in **Design** from policies already fixed here; **not** a substitute for an unsettled business rule |
| **Truly open product choice** | (None unless listed in **§15 Open Questions** with **blocking** flag) | e.g. optional second ERP mapping style—only if **§15 Open Questions** explicitly allows |

### 2.2 Manifest & Repository Traceability (Proof Matrix)

<!-- migration-note: legacy "Proof matrix: manifest + repository PRDs" preserved verbatim here under Architecture Alignment. -->

**BSS manifest (`vz-arch-manifest-bss-only.md`)**

| **Manifest requirement** | **This PRD** |
|---------------------------|--------------|
| Double-entry ledger, AR, aging, dunning (§1.2.1) | **Covers** ledger structure, AR class, posting to aging inputs |
| Posted invoices **immutable**; Credit/Debit notes (§4.4) | Posting rules S3–S4; no inline edit |
| **BillableItem** → invoice; traceability to sources | Journal entries link to billable / invoice (and related) sources |
| **Idempotent** export keys | Reconciliation flows + refs + Ledger ↔ ERP export |
| **PaymentAllocation** N:M; refund bounds | S2, S5, manifest *Payments* boundary (ledger records; orchestration in Payments) |
| **Multi-axis** identity on financial lines | Multi-tenant and line dimensions (§6.5) |
| **Dataset** separation Usage≠Rated≠Invoice | System boundaries; no rating math in this PRD |
| **Rating** `LedgerEntry` resource name | Clarify in Design: operational subledger vs Billing journal—no manifest contradiction if naming mapped |

**Repository PRD checks**

| **PRD** | **Result** |
|---------|------------|
| `PRD-billing-module-202601120119` | Aligned; this PRD narrows ledger semantics for implementation; refunds "Payments initiates; Billing records credit" aligned; consumes Rating `BillableItem` |
| `PRD-metering-pricing-module-202601120119` / Rating (§4.2) | Aligned; ledger starts at invoice post / credit note against AR |
| `PRD-subscriptions-entitlements-202601120119` | Aligned; ledger consumes posted aggregates; recurring `BillableItem` idempotency |
| `PRD-product-catalog-marketplace-202601120119` | Aligned; accounts section references Catalog `glCode`, tax category snapshots |
| `PRD-contracts-agreements-202601120119` | Aligned; ASC PO tags tie to contract artifacts |
| `PRD-billing-system-202601120119` | Consistent if module boundaries match parent doc |

**Gaps / risks**: ASC 606 not in manifest (PRD adds finance requirements; non-conflicting); duplicate scope vs billing-module (intentional specialization); `LedgerEntry` in Rating vs Billing journal (Design must resolve naming) — see §16 Risks.

## 3. Actors

### 3.1 Human Actors

#### CFO / Finance Controller

**ID**: `cpt-cf-bss-ledger-actor-cfo`

**Role**: Owns financial integrity of billing; requires auditable, ASC-ready AR, revenue, and exports.
**Needs**: Double-entry guarantees, disclosure-grade revenue recognition, GL reconciliation.

#### Revenue Assurance Analyst

**ID**: `cpt-cf-bss-ledger-actor-revenue-assurance`

**Role**: Investigates reconciliation variances and invariant alarms; owns close-blocking decisions.
**Needs**: Variance reports, exception queues, alarm routing.

#### Finance Operations

**ID**: `cpt-cf-bss-ledger-actor-finance-ops`

**Role**: Resolves suspense, unallocated cash, mapping gaps, and failed exports.
**Needs**: Exception queues, inquiry/audit pack, manual adjustment workflows.

#### Finance Approver

**ID**: `cpt-cf-bss-ledger-actor-finance-approver`

**Role**: Approves manual journals, exceptions, and dual-control actions (refunds above threshold, backdating).
**Needs**: Segregation of duties, reason codes, audit trail.

#### Auditor

**ID**: `cpt-cf-bss-ledger-actor-auditor`

**Role**: Internal/external auditor requesting tamper-evident posting history and lineage.
**Needs**: Tenant-scoped audit retrieval with full source-document linkage.

### 3.2 System Actors

#### Rating / Subscriptions

**ID**: `cpt-cf-bss-ledger-actor-rating-subscriptions`

**Role**: Produces `BillableItem` upstream of invoice post; ledger consumes posted aggregates.

#### Billing Orchestration

**ID**: `cpt-cf-bss-ledger-actor-billing-orchestration`

**Role**: Drafts invoice, calls tax, issues and posts invoices; drives per-invoice atomic posting.

#### Payments / PSP

**ID**: `cpt-cf-bss-ledger-actor-payments-psp`

**Role**: Settles funds and emits `PaymentSettled` / allocation / refund / dispute events; ledger records outcomes.

#### Tax Engine

**ID**: `cpt-cf-bss-ledger-actor-tax-engine`

**Role**: Provides authoritative `TaxBreakdown`; ledger posts, never recomputes, tax.

#### Catalog & Contracts

**ID**: `cpt-cf-bss-ledger-actor-catalog-contracts`

**Role**: Supply `glCode` snapshots, SKU/plan defaults, PO/SSP, and deferral/recognition precedence inputs.

#### ERP / GL

**ID**: `cpt-cf-bss-ledger-actor-erp-gl`

**Role**: Consumes idempotent, replay-safe exports; may be GL of record (Mode A) or downstream mirror (Mode B).

#### Recognition Run

**ID**: `cpt-cf-bss-ledger-actor-recognition-run`

**Role**: Scheduled/event-driven job releasing contract liability to revenue per active schedules.

## 4. Operational Concept & Environment

### 4.1 Module-Specific Environment Constraints

- **Multi-tenant isolation**: a single posted journal entry MUST NOT mix lines from more than one payer tenant; balances, reconciliation, export, audit, and inquiry are tenant-scoped by default. Cross-tenant aggregation requires elevated, audited context.
- **Data residency**: posted journal lines for a residency-pinned tenant (e.g. EU-only) MUST NOT cross the residency boundary in primary, replica, or DR storage; the authoritative posting clock is the in-region posting service.
- **Time**: all posting timestamps stored in **UTC**; period assignment uses the tenant fiscal-calendar timezone; local display in Presentation only.
- **Money type**: posted lines MUST use a fixed-precision decimal type (no binary float); tamper-evident authoritative store in production.

<!-- migration-note: legacy "System boundaries" ASCII flow preserved here as operational context. -->

```text
 BillableItem (Rating / Subscriptions)
        │
        ▼
  Billing Orchestration (invoice draft, tax call)
        │
        ▼
  POST INVOICE  ──▶  Ledger (double-entry)  ──▶  AR balance / aging
        │                      │
        │                      ├──▶  Recognition subledger (ASC 606)
        │                      │             ▲
        │                      │     Recognition Run (scheduled / event)
        │                      ▼
  Posted Invoice (immutable)   Export Outbound GW ──▶ ERP/GL
        ▲
        │
 PaymentApplication / Refund / CreditNote / Chargeback ──▶ Ledger (post-invoice events)
```

## 5. Scope

### 5.1 In Scope

| **Feature** | **Priority** | **Notes** |
|-------------|--------------|-----------|
| Double-entry journal engine (balanced postings, no orphan lines) | `p1` | Per-tenant + legal-entity scoping in Design |
| Chart of account classes (AR, contract liability, revenue, tax, clearing, FX, contra) | `p1` | Map to `glCode` from Catalog snapshots (manifest §4.1) |
| Posting rules: invoice issue/post, tax, payment apply, credit/debit note, refund, FX | `p1` | §6.2 (S1–S6) |
| Immutable journal store + tamper-evident audit (who/when/why, before/after refs) | `p1` | No update-in-place to posted amounts |
| AR balances & aging rollups | `p1` | Driven from posted ledger, not mutable invoices |
| ASC 606: PO tags, SSP, contract liability / recognition schedules | `p1` | §6.3 (S7) |
| Reconciliation: AR↔invoice, ledger↔ERP export, payments↔PSP | `p1` | Variance reporting |
| Multi-axis attribution on lines (resource tenant, payer, seller) | `p1` | §4.4 / §8 |
| Idempotent exports `(tenantId, invoiceId, exportTarget, transactionId)` | `p1` | Manifest §4.4 |
| Period close, idempotency matrix, SoR hierarchy, FX, exceptions, inquiry | `p1` | §6.7/§6.8/§6.10 |

### 5.2 Out of Scope

- **Rating math**, **tariff** evaluation — upstream PRDs / §4.2.
- **PSP card rails**, **webhook crypto** — §4.5 Payments; ledger **consumes** `PaymentSettled` / allocations.
- **Full ERP** as SoR — optional; manifest allows BSS SoR with **export**.
- **Detailed tax engine** rules — Tax Engine; ledger stores **posted** tax per **TaxBreakdown** evidence.
- **Bad debt**, **write-off**, and **recovery** as dedicated accounting workflows, and full **collections case management**. Commercial credit/debit notes for pricing/usage/AR adjustments remain **in scope**. Enabling bad debt later requires a separate PRD or addendum.
- **Unbilled receivable / contract asset** accounting — out of scope for MVP. This PRD covers invoice-originated **AR** and **Contract liability**. Pre-invoice ASC 606 contract assets need a separate PRD.
- **Migration / cutover from a predecessor ledger** — N/A. Billing Ledger is the first BSS component.
- **Tenant termination / wind-down** — N/A; owned by tenant-onboarding / lifecycle PRD. The ledger honors a "no new post" signal and preserves posted history.

## 6. Functional Requirements

> Posting rules S1–S6 below carry illustrative DR/CR tables; amounts are illustrative, the normative rules are the statements. **Content boundary**: DR/CR patterns and account classes here define *business posting semantics* (WHAT must post), not data models or APIs. Concrete idempotency-key field shapes (e.g. `(tenantId, invoiceId, exportTarget, transactionId)`, `pspRefundId`), storage schemas, DDL, and API contracts are illustrative of intent only and are **owned by the corresponding DESIGN** (see §17 Reference Materials); cross-reference the DESIGN doc for those specifics.

### 6.1 Double-Entry Journal Engine

#### Balanced journal entries

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-balanced-journal-entries`

The system **MUST** post every journal entry such that total debits equal total credits in the entry currency (within the rounding-unit rule), and the zero-sum invariant **MUST** hold per tenant per entry, not only globally.

**Rationale**: Balance per tenant is the foundational integrity invariant for an auditable subledger.

**Actors**: `cpt-cf-bss-ledger-actor-billing-orchestration`

<!-- migration-note: legacy "Journal entry (normative)" requirements folded into this FR. -->
Each entry MUST carry unambiguous identity; posted-at timestamp in UTC; legal-entity and currency scope; linkage to the originating business document/event; system-generated vs user-initiated marker.

#### Posting immutability

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-posting-immutability`

Posted financial facts on lines (amounts, posted account/classification, source linkage) **MUST NOT** change in place; corrections **MUST** be new compensating or reversal entries. Non-financial metadata MAY change only under controlled policy with full audit, or via append-only supplemental records.

**Rationale**: Immutability is required for SOX/ASC-grade audit and tamper evidence.

**Actors**: `cpt-cf-bss-ledger-actor-cfo`

#### Reversal canonical pattern

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-reversal-canonical-pattern`

Reversals **MUST** use **strict line-negation**: mirror the original entry's accounts and sides with negated amounts, carry explicit `reverses=<originalEntryId>` linkage, and post at current effective time. Storno and gross-replace **MUST NOT** be the BSS ledger shape (ERP-side mapping permitted as presentation only).

**Rationale**: One canonical reversal shape keeps exports consistent and auditable.

**Actors**: `cpt-cf-bss-ledger-actor-finance-ops`

#### Account classes (posting reference)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-account-classes`

Posted lines **MUST** classify against the following business account classes; concrete GL numbers are tenant/ERP configured, BSS stores **class** + `glCode` snapshot from Catalog §4.1.

**Rationale**: A fixed class taxonomy is required so posting rules and reconciliation are unambiguous.

**Actors**: `cpt-cf-bss-ledger-actor-catalog-contracts`

<!-- migration-note: legacy "Accounts (account classes)" + "Books (logical)" tables preserved verbatim. -->

| **Class** | **Typical normal balance** | **Role** |
|-----------|---------------------------|----------|
| **Accounts Receivable** | Debit | Unpaid invoice balances; reduced **only** when payment is **allocated** (not merely received). |
| **Unallocated cash (suspense / unapplied pool)** | Credit | Settled cash not yet allocated to specific posted invoices. Cleared by allocation to AR, refund, or transfer to reusable customer credit per policy. Do not use to mean intentional on-account credit unless the business event says so. |
| **Reusable customer credit (on-account)** | Credit | Optional separate class — only when product policy isolates wallet / on-account balances. If a single bucket is used, metadata + event type MUST preserve the Glossary split, with per-event-type running sub-balances. A single bucket without sub-balance tracking is non-compliant. |
| **Contract liability** | Credit | Unrecognized revenue for prepaid or multi-period performance obligations (*deferred revenue* = synonym). |
| **Revenue** | Credit | Recognized revenue (usage, recurring, professional services). |
| **Tax payable / recoverable** | Per jurisdiction | From **TaxBreakdown**; never recomputed from mutable catalog post-post. |
| **Cash / bank clearing** | Debit | PSP settlement and inbound funds before/while routing to unallocated vs AR. |
| **Refund clearing / liability** | Varies | Bridge to Refund until PSP confirms; dual-control refunds §4.5. |
| **FX gain/loss** | Varies | Rounding and rate differences per locked policy. |
| **Discount / contra-revenue** | Debit | Commercial credits that reduce recognized revenue (S3): normative debit here, not a direct debit to Revenue. |

Logical books: **AR (subledger)**, **Revenue / contract liability**, **Tax**, **Cash / clearing** (unapplied funds until allocated; PaymentAllocation links applied amounts), **FX**, **Contra / adjustments** (bad-debt allowance/write-off/recovery are out of scope).

### 6.2 Posting Rules — Invoice, Payment, Adjustments, Refund (S1–S5)

#### S1 — Invoice post (direct split)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-invoice-post-direct-split`

On invoice post, the ledger **MUST** use the **direct split** pattern: debit **AR** for the full invoice (incl. tax), credit **Revenue** for the recognized-at-post portion and **Contract liability** for the deferred portion (ex-tax) per PO/policy, credit **Tax payable** per `TaxBreakdown`. The system **MUST NOT** use gross-Revenue-then-same-invoice-reclassification as the default. When nothing is deferred, Contract liability lines **MUST** be absent (zero-amount placeholders rejected at post-time validation).

**Rationale**: A single deferral pattern guarantees balance and ASC 606 substance without ERP-specific reclass.

**Actors**: `cpt-cf-bss-ledger-actor-billing-orchestration`

| **Line** | **Debit** | **Credit** |
|----------|-----------|------------|
| Recognize AR | AR (incl. tax) | |
| Revenue (recognized at post) | | Revenue (ex-tax) |
| Contract liability (deferred per PO) | | Contract liability (ex-tax) |
| Tax | | Tax payable |

Rules: amounts MUST match sum(InvoiceItem) + TaxBreakdown with rounding evidence stored once; item lines MUST carry `glCode`, `skuId`/`planId`/`priceId`, `pricingSnapshotRef`; no post without a balanced entry; failed post MUST roll back with no partial `posted` state.

#### S2 — Payment: settlement vs allocation

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-payment-settlement-vs-allocation`

Settlement (funds confirmed) and allocation (which posted invoices a payment satisfies) **MUST** be representable as distinct patterns whenever unallocated balances can exist. **AR MUST** decrease only when allocations are applied, not from receipt alone — except a narrow atomic settle-and-apply shortcut with no residual unallocated balance.

**Rationale**: Prepayments, partial pay, multi-invoice application, and overpayments require separating cash location from AR application.

**Actors**: `cpt-cf-bss-ledger-actor-payments-psp`

| **Step** | **Debit** | **Credit** |
|----------|-----------|------------|
| A — Settlement | Cash / clearing | Unallocated cash (unapplied pool) |
| B — Allocation | Unallocated cash (unapplied pool) | AR |

Rules: partial multi-invoice allocation reduces open AR per invoice by allocated amount; overpayment remainder stays unallocated until refund, future allocation, or explicit on-account election; prepayment uses Pattern A until invoice post; FX lines when functional ≠ payment currency.

#### S3 — Credit note (adjustment)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-credit-note-adjustment`

Credit notes reducing recognized revenue **MUST** debit **Contra-revenue** (not Revenue directly); the unreleased deferred portion **MUST** debit **Contract liability**, split from the targeted posted invoice item, PO/allocation group, and recognition-schedule state. A credit note **MUST NOT** reduce AR while leaving related unreleased contract liability unchanged. If the recognized-vs-unreleased split cannot be determined unambiguously, the post **MUST** block and route to the exception queue. AR-only goodwill credits debit a non-revenue class (not bad-debt/write-off).

**Rationale**: Keeps Revenue gross, supports disclosure-grade exports, and preserves ASC deferral lineage.

**Actors**: `cpt-cf-bss-ledger-actor-finance-ops`

| **Line** | **Debit** | **Credit** |
|----------|-----------|------------|
| Reduce recognized revenue (ex-tax) | Contra-revenue | |
| Reduce unreleased deferred amount (ex-tax) | Contract liability | |
| Reverse tax | Tax payable | |
| Reduce AR (incl. tax) | | AR |

#### S4 — Debit note (additional charge post-fact)

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-fr-debit-note-charge`

A debit note for additional post-invoice charges **MUST** credit **Tax payable** per posted `TaxBreakdown` and follow the same direct split as S1 (Revenue recognized portion, Contract liability deferred portion ex-tax). It **MUST NOT** change posted invoice line rows (compensating entry only) and **MUST** be a balanced entry that rolls back fully on failure.

**Rationale**: Post-fact charges must reflect full customer balance impact including tax, consistently with invoice post.

**Actors**: `cpt-cf-bss-ledger-actor-billing-orchestration`

#### S5 — Refund (balance-first)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-refund-balance-first`

Refund posting **MUST** follow which balance is returned: **Unallocated cash** / **Reusable customer credit** (Pattern A — no P&L impact) or **AR** restoration (Pattern B). Refund JEs **MUST NOT** debit Revenue or Contra-revenue; recognized revenue and material tax reversals follow S3/S4 (S3+S5 pairing in jurisdiction-expected order). Where initiation and PSP settlement are not atomic, refunds **MUST** post two stages against **Refund clearing**; single-step is permitted only when atomic with no clearing residual.

**Rationale**: Prevents revenue distortion from cash mechanics and makes never-confirmed refunds visible as clearing balances.

**Actors**: `cpt-cf-bss-ledger-actor-payments-psp`

| **Situation** | **Debit** | **Credit** |
|---------------|-----------|------------|
| Unallocated / on-account credit refunded | Unallocated cash (or customer-credit pool) | Cash / clearing |
| Applied payment refunded (AR restored) | AR | Cash / clearing |

Rules: aggregate cap Σ(refunds) ≤ settled amount; per-invoice (Pattern B) cap ≤ amount previously allocated from that payment to that invoice; dual-control above threshold; idempotent per PSP refund id; refund-clearing aging alarms; PSP-rejected/voided refunds reverse stage 1 via strict line-negation.

#### S3/S5 credit-note cumulative cap

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-credit-note-cumulative-cap`

For each posted invoice, a new credit note (incl. tax) **MUST NOT** exceed available headroom = `original posted total + Σ(prior S4 debit notes) − Σ(prior credit notes)` (all incl. tax). Over-cap adjustments **MUST** route via non-revenue debit or be out of scope under bad-debt workflows — never silently allowed via S3.

**Rationale**: Prevents over-crediting an invoice beyond its real exposure.

**Actors**: `cpt-cf-bss-ledger-actor-finance-approver`

#### Allocation precedence (multi-invoice payment)

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-fr-allocation-precedence`

Multi-invoice allocation order **MUST** be deterministic: default oldest posting date first, ties broken by smallest invoice id; tenant overrides allowed. Statutory jurisdiction rules (e.g. UK Consumer Credit Act) are **🔄 out of v1 scope (amended 2026-06-11, superseding the 2026-06-10 "UK CCA only" ratification)**: the customer-instructed tenant override is the B2B compliance path, and a data-driven jurisdiction→rule registry — which **would** take precedence over both tenant overrides and the platform default — is added post-MVP only if Legal names a market whose payers a statutory regime binds. Owners: Legal + PM. Design: `design/03-payments-allocation.md` § Needs Discussion, "Statutory allocation-rule registry"; statutory rules **MUST NEVER** live in the ledger when the registry arrives (external AR-policy component issues an explicit split; the ledger validates and records).

**Rationale**: Deterministic, statute-compliant allocation is required for correct AR and disputes.

**Actors**: `cpt-cf-bss-ledger-actor-payments-psp`

### 6.3 ASC 606 Revenue Recognition

#### S6 — Recognition schedule controls

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-recognition-schedule-controls`

Recognition runs **MUST** release amounts per a documented schedule (DR Contract liability / CR Revenue), **MUST NOT** double-recognize the same deferred slice for the same period/segment (idempotent, at-most-once per segment per period), and schedule changes **MUST** be controlled (approval, audit trail, new version or compensating entries) — never silent rewrites of past releases.

**Rationale**: Controlled, idempotent recognition is the core ASC 606 integrity guarantee.

**Actors**: `cpt-cf-bss-ledger-actor-recognition-run`

#### ASC 606 PO identification & transaction-price

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-asc606-po-identification`

Posted invoice items **MUST** carry PO or allocation-group identifiers when the line has deferral/recognition, is marked multi-PO/ASC-tracked/SSP, or has variable consideration; otherwise a Catalog default allocation group for traceability. Outside the narrow immaterial-one-shot exemption, missing PO/allocation group **MUST** block post. SSP snapshots are required for multi-PO allocation; deferral and recognition timing **MUST** be derivable from Contract → Catalog → PO type → billing model precedence.

**Rationale**: PO-level economics and SSP allocation are mandatory for ASC 606 audit.

**Actors**: `cpt-cf-bss-ledger-actor-catalog-contracts`

#### Revenue-stream disaggregation

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-revenue-stream-disaggregation`

Every revenue-affecting journal line (and originating posted invoice item where applicable) **MUST** carry a mandatory revenue-stream classification (usage / recurring / one-time, or Design-equivalent), preserved via distinct natural accounts, sub-accounts, or reporting dimensions — not free text alone. Mixed invoices **MUST** split amounts by stream.

**Rationale**: Disclosure-grade reporting requires disaggregated, machine-sliceable revenue.

**Actors**: `cpt-cf-bss-ledger-actor-cfo`

#### Recognition audit linkage

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-recognition-audit-linkage`

Every recognition journal entry **MUST** carry minimum linkage: recognition period/segment; PO or allocation group; deferral origin resolving to posted invoice item(s); subscription/entitlement context when schedule-scoped. Period alone is insufficient; non-invoice deferral sources are out of MVP scope and **MUST** block.

**Rationale**: Auditable lineage from recognition back to invoiced deferral is mandatory.

**Actors**: `cpt-cf-bss-ledger-actor-auditor`

### 6.4 Immutability, Audit & Compliance

#### Immutable audit logs & tamper evidence

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-immutable-audit-logs`

The production authoritative posted-journal store **MUST** be tamper-evident (hash chain, WORM, signed append log, or equivalent per Design). Financially binding fields **MUST NOT** be updated/deleted in place; retention **MUST** meet legal/contractual minimums (manifest §4.4, typically ≥ 7 years). Operational surfaces **MUST NOT** carry direct PII; the secured audit store holds investigation-grade records.

**Rationale**: Tamper evidence and PII minimization are compliance show-stoppers.

**Actors**: `cpt-cf-bss-ledger-actor-auditor`

#### Audit retrieval (tenant-scoped)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-audit-retrieval`

For a posted journal entry, the system **MUST** make who/when/source-document linkage/correlation retrievable, and responses **MUST** be tenant-scoped; cross-tenant access requires elevated context recorded with actor, reason, and scope.

**Rationale**: Auditors and support need lineage without cross-tenant leakage.

**Actors**: `cpt-cf-bss-ledger-actor-auditor`

#### Right-to-erasure vs retention

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-fr-right-to-erasure`

Posted journal lines reference an immutable internal payer id, never raw PII. After erasure, journal lines remain intact and queryable; reverse-lookup to human PII **MUST** yield a tombstone marker without breaking ledger integrity or tamper-evidence chains.

**Rationale**: Reconciles GDPR Art. 17 with financial retention and immutability.

**Actors**: `cpt-cf-bss-ledger-actor-cfo`

### 6.5 Multi-Tenant & Multi-Axis Posting

#### Tenant isolation in posting

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-tenant-isolation-posting`

A single posted journal entry **MUST NOT** mix lines from more than one payer tenant (legal-entity mixing equally forbidden); tenant chart-of-accounts **MUST NOT** leak across tenants; balances, reconciliation, export, audit, and inquiry are tenant-scoped by default.

**Rationale**: Tenant isolation is a hard multi-tenant financial boundary.

**Actors**: `cpt-cf-bss-ledger-actor-billing-orchestration`

#### Multi-axis attribution

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-fr-multi-axis-attribution`

Every posted line **MUST** carry the applicable tenant axes: payer tenant (AR), seller tenant (channel/reseller lines), resource tenant (showback). Seller is line-level metadata, not a tenant-scope split; inter-tenant/reseller settlement is out of scope here.

**Rationale**: Multi-axis attribution enables showback and channel splits without breaking isolation.

**Actors**: `cpt-cf-bss-ledger-actor-rating-subscriptions`

### 6.6 Idempotency & Replay

#### Idempotency per business flow

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-idempotency-per-flow`

Each flow **MUST** be idempotent per its business key: invoice post (per posted invoice), settlement (per PSP event id), allocation (per allocation id), credit/debit note (per note id), refund (per `(tenant, PSP refund id, phase)`), recognition (per segment × period), chargeback (per `(tenant, payment/dispute id, outcome phase)`), ERP export (per export key).

**Rationale**: At-most-once ledger effect under at-least-once delivery is mandatory.

**Actors**: `cpt-cf-bss-ledger-actor-payments-psp`

#### Idempotent-replay response contract

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-idempotent-replay-contract`

Replay with same key + same payload **MUST** return the prior posting reference (entry id, posted-at, status); replay with same key + different payload **MUST** hard-error with a specific code and capture the conflicting payload in the secured audit store. Business identifiers **MUST** dedupe for the financial retention period.

**Rationale**: Callers need deterministic replay semantics, not generic acks.

**Actors**: `cpt-cf-bss-ledger-actor-rating-subscriptions`

### 6.7 Reconciliation & Period Close

#### AR tie-out

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-ar-tie-out`

AR ledger balance per payer **MUST** tie (within tolerance) to the derived AR projection from all AR-affecting posted facts: invoice posts, debit notes, payment allocations, credit notes, applied-payment refunds, chargeback Won/Lost/Partial outcomes, CreditApplication wallet-to-AR settlements, and reversals. "Dispute opened" sub-class moves are AR-class-neutral and **MUST NOT** enter the roll-up.

**Rationale**: AR truth must be derivable from posted facts, not an independent mutable view.

**Actors**: `cpt-cf-bss-ledger-actor-revenue-assurance`

#### ERP / GL export idempotency

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-erp-export-idempotency`

Export **MUST** be idempotent and replay-safe: a successful replay is indistinguishable (same business amounts to GL) from the first success. BSS owns re-drive with the same key; failed exports are queued, never dropped; ERP "already posted" is treated as idempotent success; mismatched payload for the same key **MUST** alarm and block.

**Rationale**: Idempotent export is a manifest §4.4 invariant and reconciliation precondition.

**Actors**: `cpt-cf-bss-ledger-actor-erp-gl`

#### Accounting periods and close

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-accounting-periods-close`

Closed periods **MUST NOT** accept new routine postings except reversals or authorized corrections (dual-control). Period close **MUST** block while reconciliation variance exceeds tolerance or required exception queues remain open. Material backdating requires exception approval with audit trail; backdating thresholds outside [1, 30] business days **MUST** be rejected at config time.

**Rationale**: Period integrity and controlled close are core finance controls.

**Actors**: `cpt-cf-bss-ledger-actor-finance-approver`

#### Exceptions, suspense & reconciliation handling

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-fr-exception-suspense-handling`

Unmatched settled payments **MUST** remain in unallocated (aged with alerts); missing account mapping **MUST** block post or route to suspense (no silent wrong-revenue mapping); export failures **MUST** retry with no silent drop; reconciliation mismatch **MUST** produce a variance report + ticket and block close above tolerance.

**Rationale**: No financial fact may be silently dropped or mis-mapped.

**Actors**: `cpt-cf-bss-ledger-actor-finance-ops`

### 6.8 Money, Rounding & Foreign Exchange

#### Money type, rounding & decimal scale

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-money-rounding-scale`

Posted amounts **MUST** use a fixed-precision decimal at the currency's ISO 4217 minor-unit scale with **banker's rounding (half-to-even)** as platform default, applied identically across S1–S6 and exports. Internal compute may carry up to 4 extra decimals but **MUST NOT** leak into posted truth; tenant rounding override only with recorded, audited evidence; over-range **MUST** hard-error.

**Rationale**: Deterministic money arithmetic is auditor-verifiable and prevents drift.

**Actors**: `cpt-cf-bss-ledger-actor-cfo`

#### Negative-balance invariants & alarms

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-negative-balance-invariants`

The ledger **MUST** emit production alarms (not just logs) on sign violations per the class table: Cash/clearing, Unallocated cash, Reusable customer credit, AR (per `(payer, invoice)` and payer-aggregate), and Contract liability **MUST NOT** go negative; Tax payable may go negative during reversal windows evaluated per `(jurisdiction, filing-period)`. Single-bucket designs **MUST** alarm on per-event-type sub-balances.

**Rationale**: Sign violations indicate posting defects with direct financial-integrity impact.

**Actors**: `cpt-cf-bss-ledger-actor-revenue-assurance`

#### Multi-currency & FX

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-multi-currency-fx`

Rate locks **MUST** follow the normative lock points: S1 at post, S2 on settle and (if different) on alloc, S6 does not re-lock the revenue line. Realized FX **MUST** post on receipt/settlement/allocation/refund/chargeback when document ≠ functional currency; unrealized revaluation is optional and, if on, must be dedicated, idempotent, and reversible (no silent S1 recompute).

**Rationale**: Explicit FX lock points and realized/unrealized treatment are required for correct multi-currency AR.

**Actors**: `cpt-cf-bss-ledger-actor-cfo`

#### FX rate-source failure & staleness

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-fr-fx-rate-source-failure`

When the rate provider is unreachable or the latest rate is stale, the post **MUST** block by default, or proceed only with a `stale=true` snapshot where tenant policy explicitly allows; silent fallback to a last-good rate without the marker **MUST NOT** happen. Snapshots are immutable; later provider revisions post as new compensating entries.

**Rationale**: Prevents silent posting on missing/stale rates and protects historical immutability.

**Actors**: `cpt-cf-bss-ledger-actor-finance-ops`

### 6.9 Chargebacks & Disputes

#### Chargeback and dispute outcomes

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-chargeback-dispute-posting`

For PSP disputes/chargebacks, postings **MUST** match the normative outcomes (opened/hold, won, lost, partial-or-split) without editing original payment JEs; "Dispute opened" reclassification **MUST NOT** zero/negate `(payer, invoice)` AR or change payer-aggregate AR sign. Replay of the same `(tenant, payment/dispute id, outcome phase)` key **MUST** be a no-op. P&L (if any) routes via S3, not bad-debt workflows.

**Rationale**: Disputes must reconcile to PSP outcomes without double-posting or AR distortion.

**Actors**: `cpt-cf-bss-ledger-actor-payments-psp`

### 6.10 Lifecycle, Ordering & Governance

#### Account lifecycle posting

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-fr-account-lifecycle-posting`

Collections suspension **MUST NOT** freeze recognition or allocation; cancelled subscription recognition is driven by the upstream schedule decision (ledger consumes, not decides); closed payer accounts with open AR permit compensating posts only and block new invoice posts (closing with non-zero balance requires approval + audit marker).

**Rationale**: Distinct lifecycle states must not be conflated in posting behavior.

**Actors**: `cpt-cf-bss-ledger-actor-finance-ops`

#### Out-of-order event handling

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-fr-out-of-order-event-handling`

Allocation before settlement **MUST** queue (not reject) and apply on settlement; refund before original payment **MUST** quarantine; recognition for period N before N−1 **MUST** order by `(schedule, period)`; out-of-order chargeback phases **MUST** queue rather than post partial outcomes. Source clock skew beyond ±15 min warns, ±24 h alerts and quarantines.

**Rationale**: Ordering guarantees prevent incorrect or partial financial state.

**Actors**: `cpt-cf-bss-ledger-actor-payments-psp`

#### Manual journals & adjustment governance

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-fr-manual-adjustment-governance`

Normal JEs are system-generated from S1–S6 and dispute/FX runs; free-form GL vouchers are out of scope for MVP. Governed adjustments (suspense, rounding) **MUST** enforce segregation of duties (preparer vs approver), amount/entity thresholds with dual-control, and mandatory reason code + actor + before/after audit.

**Rationale**: Controls prevent ad-hoc re-keying of settled economic facts.

**Actors**: `cpt-cf-bss-ledger-actor-finance-approver`

#### Policy versioning & historical immutability

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-fr-policy-versioning-immutability`

Posted lines and idempotent export keys for completed periods **MUST** remain immutable when future-effective changes alter contract, catalog, tax, or revenue rules; new documents use the policy/snapshot versions in effect at their own posting; corrections to past amounts use only new compensating entries — no hidden recompute of closed history.

**Rationale**: Historical immutability under policy change is required for audit and ASC 606.

**Actors**: `cpt-cf-bss-ledger-actor-cfo`

## 7. Non-Functional Requirements

### 7.1 NFR Inclusions

> PM Team owns commitment of ledger-specific NFR numbers; Architecture, Finance, DR, DPO, and Audit contribute during Design. Rows below marked TBD MUST be committed before Design starts (see §15 Open Questions).

#### Availability of the posting path

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-nfr-availability`

The billing posting path **MUST** sustain ≥ 99.9% availability.

**Threshold**: ≥ 99.9% monthly availability for the posting path.

**Rationale**: Posting is on the revenue-critical path; downtime blocks invoicing and settlement.

#### Disaster recovery (RTO / RPO)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-nfr-rto-rpo`

The ledger **MUST** meet RTO ≤ 60 minutes and RPO ≤ 5 minutes per region.

**Threshold**: RTO ≤ 60 min; RPO ≤ 5 min per region.

**Rationale**: Financial facts cannot be lost beyond a tight recovery bound.

#### Data residency & geo-redundancy

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-nfr-data-residency`

Posted journal lines for a residency-pinned tenant **MUST NOT** cross the residency boundary in primary, replica, or DR storage; minimum multi-AZ, with cross-region warm/active per tenant policy.

**Threshold**: Zero cross-boundary storage for residency-pinned tenants; multi-AZ minimum.

**Rationale**: Regulatory data-residency obligations are enforceable at tenant scope.

#### Posting & read performance

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-nfr-posting-performance`

Interactive posting and AR reads **MUST** meet committed latency targets; bill-run throughput **MUST** sustain manifest baselines.

**Threshold**: Draft targets (to be committed): AR balance warm read p95 ≤ 200 ms; operator-initiated single balanced post p95 ≤ 500 ms; bill run ≥ thousands invoices/min and ≤ 60 min per 100k invoices. Recognition run window, ERP export SLA, and period close window: TBD.

**Rationale**: Performance bounds the engineering shape and capacity planning.

#### Tamper-evidence verification cadence

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-nfr-tamper-evidence-cadence`

The ledger **MUST** run periodic chain-integrity verification and alert on the first inconsistency.

**Threshold**: Verification at a committed cadence (TBD); alert on first inconsistency.

**Rationale**: Tamper evidence is only meaningful if actively verified.

### 7.2 NFR Exclusions

- **End-user-facing UX performance NFRs**: Not applicable — customer-facing transaction history is owned by Invoice/Payment services (SoR), not the ledger; the ledger serves finance/audit/reconciliation surfaces only.
- **PSP/network throughput NFRs**: Not applicable — settlement rails and webhook performance are owned by Payments (§4.5); the ledger consumes settled events.

#### Cross-domain applicability dispositions

Explicit dispositions for checklist domains not otherwise addressed (no silent omissions):

- **Authentication**: Not applicable in this PRD — user/service authentication is owned by the platform Identity Provider (Common Core); the ledger trusts an already-authenticated, tenant-scoped context and adds **authorization** controls (RBAC, segregation of duties, dual-control) in §6.10.
- **Safety (ISO/IEC 25010 §4.2.9)**: Not applicable — the Billing Ledger is a pure information/financial system with no physical actuation, medical, or human-safety hazard surface.
- **Accessibility (WCAG) & Internationalization**: Not applicable to this backend PRD — the finance/audit/reconciliation surfaces in §11 are rendered by the frontend layer; accessibility (WCAG 2.2 AA) and i18n/localization requirements are owned by the corresponding frontend DESIGN, not this subledger PRD.
- **Offline / device-platform NFRs**: Not applicable — server-side, always-connected subledger.

## 8. Five Quality Vectors Analysis

| **Quality Vector** | **Show-Stopper Requirements** | **Rationale** |
|--------------------|-------------------------------|---------------|
| **Efficiency** | Bill-run posting MUST sustain manifest throughput baselines (≥ thousands invoices/min) without violating balance/idempotency. | A ledger that cannot keep up with bill runs blocks revenue. |
| **Reliability** | Every entry balanced, idempotent per business key, with at-most-once ledger effect and no silent drop of financial facts. | Financial correctness and replay safety are non-negotiable. |
| **Performance** | AR read and interactive post latency within committed p95 targets. | Operators and inquiry surfaces require responsive posting/reads. |
| **Security** | Tenant isolation in every entry, tamper-evident store, PII minimization, RBAC + dual-control for manual/refund actions. | Multi-tenant financial data demands isolation, integrity, and least-privilege. |
| **Versatility** | Configurable per-tenant rounding override, FX provider order, ERP operating mode (A/B), and fiscal-calendar timezone — without breaking S1–S6 invariants. | Enterprises require policy configurability without compromising core invariants. |

## 9. Public Library Interfaces

> The Billing Ledger is a backend subledger, not a client library. Interfaces below are high-level contracts; concrete API schemas, endpoints, and DDL belong in DESIGN (PRD content boundary).

### 9.1 Public API Surface

#### Ledger inquiry & audit-pack interface

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-interface-ledger-inquiry`

**Type**: REST / query API (shape in Design)

**Stability**: stable (contract intent), schema unstable (Design owns)

**Description**: Tenant-scoped retrieval of balances, journal entries, and source-document lineage; audit-pack export with full linkage chain.

**Breaking Change Policy**: Major version bump for incompatible query/response changes.

#### Posting intake interface

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-interface-posting-intake`

**Type**: event/command intake (shape in Design)

**Stability**: stable (contract intent)

**Description**: Accepts invoice-post, payment settlement/allocation, credit/debit note, refund, recognition, and chargeback events; enforces idempotency and balance.

**Breaking Change Policy**: Major version bump; idempotency-key semantics are part of the contract.

### 9.2 External Integration Contracts

#### ERP / GL export contract

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-contract-erp-export`

**Direction**: provided by ledger to ERP/GL

**Protocol/Format**: idempotent export with `(tenantId, invoiceId, exportTarget, transactionId)` key; ack required (Design)

**Compatibility**: Replay-safe; identical business amounts on retry; mismatched-payload-for-same-key is a hard error.

#### Payments / PSP event contract

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-contract-payments-events`

**Direction**: required from Payments

**Protocol/Format**: `PaymentSettled`, `PaymentAllocation`, refund, and dispute events with stable idempotency keys (Design)

**Compatibility**: At-least-once delivery; ordering within `(tenantId, aggregateId)`; idempotent by PSP transaction/refund id.

#### Tax / Catalog / Contracts input contract

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-contract-tax-catalog-contracts`

**Direction**: required from Tax Engine, Catalog, Contracts

**Protocol/Format**: `TaxBreakdown` evidence, `glCode`/SSP snapshots, PO/recognition precedence inputs (Design)

**Compatibility**: Immutable snapshot references; ledger posts, never recomputes, supplied evidence.

## 10. Use Cases

#### Ledger inquiry and audit-pack export

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-usecase-ledger-inquiry`

**Actor**: `cpt-cf-bss-ledger-actor-auditor`

**Preconditions**:
- Posted journal entries exist for the tenant and period.

**Main Flow**:
1. Filter by payer, period, account class, legal entity.
2. Drill balance → journal entry → source document (invoice, payment, note, recognition).
3. Export audit pack (CSV/PDF) with full linkage chain for external auditors.

**Postconditions**:
- Tenant-scoped audit pack produced with complete lineage.

**Alternative Flows**:
- **Cross-tenant rollup requested**: requires elevated context recorded with actor, reason, and scope.

#### Reconciliation review and close decision

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-usecase-reconciliation-review`

**Actor**: `cpt-cf-bss-ledger-actor-revenue-assurance`

**Preconditions**:
- Reconciliation job has run for the period scope (AR, PSP, ERP).

**Main Flow**:
1. Select period / scope.
2. Review variances vs tolerance.
3. Acknowledge, assign owner, or block close escalation.

**Postconditions**:
- Variances triaged; close blocked when above tolerance.

**Alternative Flows**:
- **Variance above tolerance**: period close is blocked until resolved or approved exception.

#### Exception queue resolution

- [ ] `p2` - **ID**: `cpt-cf-bss-ledger-usecase-exception-resolution`

**Actor**: `cpt-cf-bss-ledger-actor-finance-ops`

**Preconditions**:
- Unallocated cash, suspense lines, failed exports, or mapping gaps exist.

**Main Flow**:
1. Review aged unallocated cash, suspense lines, failed exports, mapping gaps.
2. Resolve or approve exception per policy.

**Postconditions**:
- Exceptions resolved or escalated with audit trail.

## 11. User Interaction and Design

| **Interface Name** | **Role** | **Steps** | **Mockup Screen** |
|--------------------|----------|-----------|-------------------|
| Ledger inquiry | Finance / Audit | 1. Filter by payer, period, account class, legal entity<br>2. Drill balance → journal entry → source document (invoice, payment, note, recognition)<br>3. Export audit pack (CSV/PDF) with full linkage chain | — |
| Reconciliation dashboard | Revenue Assurance | 1. Select period / scope (AR, PSP, ERP)<br>2. Review variances vs tolerance<br>3. Acknowledge, assign owner, or block close escalation | — |
| Exception queue | Operations / Finance | 1. Unallocated cash age, suspense lines, failed exports, mapping gaps<br>2. Resolve or approve exception per policy | — |

## 12. Acceptance Criteria

### Double-entry

**1. Balance**
- **Given** any journal entry
- **When** posted
- **Then** debits MUST equal credits in **entry currency** (within rounding-unit rule)

**2. Immutability**
- **Given** a posted journal line
- **When** a correction is required
- **Then** the system MUST create a **new** compensating or reversal entry
- **And** MUST NOT mutate the original line amounts

### Invoice and adjustments

**3. Posted invoice**
- **Given** `Invoice.status=posted`
- **When** a commercial correction is applied
- **Then** the correction MUST flow through **CreditNote** or **DebitNote**
- **And** MUST produce ledger entries linked to source documents

**4. Deferred revenue at invoice post**
- **Given** an invoice post that includes deferred performance obligations
- **When** the invoice journal entry is posted
- **Then** the ledger MUST use the **direct split** pattern (credit Revenue for recognized-at-post, credit Contract liability for deferred, debit AR for the full amount)
- **And** MUST NOT rely on gross Revenue plus same-invoice reclassification as default
- **And** when no amount is deferred, the entry MUST NOT contain Contract liability lines (zero-amount placeholders rejected at post-time validation)

### Payments

**5. Cash receipt vs allocation (S2)**
- **Given** settled inbound funds that may or may not yet be allocated
- **When** the ledger records payment activity
- **Then** settlement and allocation MUST be representable as distinct patterns whenever unallocated balances can exist
- **And** AR MUST decrease only when allocations are applied, except a narrow atomic settle-and-apply shortcut with no residual

**6. Refund posting (S5)**
- **Given** an approved cash refund
- **When** the ledger posts the refund
- **Then** debits MUST hit unallocated cash (or reusable customer credit) for on-account/overpayment returns, and AR when reversing applied payments — without Revenue or contra-revenue
- **And** recognized revenue and material tax reversals MUST follow S3/S4, with S3+S5 pairing in jurisdiction-expected order
- **And** non-atomic initiation/settlement MUST use the two-stage Refund clearing pattern

### Reconciliation

**7. AR tie-out**
- **Given** a payer account
- **When** reconciliation runs
- **Then** the AR ledger balance MUST match the derived AR projection within tolerance (Dispute-opened reclassification excluded from the roll-up)

### Audit

**8. Audit retrieval**
- **Given** a posted journal entry
- **When** an auditor requests history
- **Then** who/when/source-document linkage/correlation MUST be retrievable
- **And** the response MUST be tenant-scoped; cross-tenant access requires elevated, recorded context

### Revenue recognition

**9. Recognition schedule controls (S6)**
- **Given** deferred balances under active recognition schedules
- **When** a recognition run executes for a period
- **Then** released amounts MUST follow the defined schedule
- **And** the same deferred slice MUST NOT be recognized twice for the same period/segment
- **And** schedule changes MUST be controlled (no silent rewrites of past releases)

### High-risk and cross-cutting

**10. Chargeback and dispute outcomes**
- **Given** a PSP dispute/chargeback event with a stable idempotency key
- **When** the ledger records opened/won/lost/partial-or-split
- **Then** postings MUST match the normative table (hold vs AR vs Cash; no in-place edit of original payment JEs)
- **And** replay of the same key MUST be a no-op

**11. Accounting periods and close**
- **Given** a period is closed or locked
- **When** a backdated or in-period post is attempted
- **Then** the system MUST enforce closed-period and reversal policies (no silent bypass)

**12. ERP / GL export idempotency**
- **Given** an export with an idempotency key
- **When** the export is retried after a transient failure
- **Then** a successful replay MUST be indistinguishable (same business amounts to GL) from the first success

**13. Multi-currency and foreign exchange**
- **Given** multi-currency activity
- **When** settle, alloc, refund, or revaluation runs
- **Then** realized vs unrealized FX and rate-lock points MUST follow the normative FX rules

**14. Manual journals and adjustments**
- **Given** a post not from S1–S6 and default system-generated paths
- **When** a user-initiated or imported manual line is proposed
- **Then** it MUST follow manual-journal and adjustment-governance rules (not a stand-in for the S1–S6 subledger)

**15. Policy versioning and historical immutability**
- **Given** a new posting or GL-mapping policy version
- **When** the version is activated
- **Then** posted lines and idempotent export keys for completed periods MUST remain immutable (no silent recompute of closed history)

**16. Money, rounding, and decimal scale**
- **Given** any posting in any flow (S1–S6)
- **When** the entry is committed
- **Then** posted amounts MUST conform to currency scale; banker's rounding default; tenant override only with recorded evidence
- **And** internal compute scale MUST NOT leak into posted truth

**17. Negative-balance invariants**
- **Given** the account class sign rules
- **When** a posting would produce a violation
- **Then** the system MUST emit a production alarm and route to Revenue Assurance
- **And** classes that MAY go negative within bounds MUST alarm only when bounds are exceeded

**18. FX rate-source failure**
- **Given** a multi-currency posting
- **When** the rate provider is unreachable or the latest rate is stale
- **Then** the post MUST block by default, or proceed with a `stale=true` snapshot only where tenant policy allows
- **And** silent fallback to a last-good rate without the marker MUST NOT happen

**19. Idempotent-replay response contract**
- **Given** an idempotency key previously posted
- **When** the same key is replayed with identical payload
- **Then** the response MUST return the prior posting reference
- **And** conflicting-payload replay MUST hard-error with a specific code and capture the payload in the secured audit store

**20. Tenant fiscal-calendar period boundary**
- **Given** a tenant with a non-UTC fiscal-calendar timezone
- **When** a posting falls near local month-end
- **Then** period assignment MUST use the tenant's fiscal-calendar timezone, not UTC

**21. Account lifecycle posting**
- **Given** a payer account in suspended or closed state
- **When** a posting is attempted
- **Then** the system MUST follow per-state rules (suspension does not freeze recognition/allocation; closure permits compensating posts only, blocks new invoice posts)

**22. Right-to-erasure vs immutability**
- **Given** an erasure request for a payer
- **When** the secured audit store applies the erasure
- **Then** posted journal lines and internal references MUST remain intact and queryable
- **And** reverse-lookup to human PII MUST yield a tombstone marker without breaking ledger integrity

**23. Reversal canonical pattern**
- **Given** a posted journal entry that requires reversal
- **When** the ledger posts the reversing entry
- **Then** it MUST use strict line-negation (same accounts/sides, negated amounts, explicit `reverses=` linkage, current effective time)
- **And** storno and gross-replace MUST NOT be the BSS ledger shape

**24. Credit-note cumulative cap**
- **Given** one or more credit notes posted against a single posted invoice
- **When** an additional credit note is proposed
- **Then** the new credit note (incl. tax) MUST NOT exceed available headroom `original posted total + Σ(prior S4) − Σ(prior S3)`
- **And** unreleased deferred portions MUST reduce Contract liability, not only Contra-revenue
- **And** the recognized-vs-unreleased split basis MUST be derived and stored; ambiguous splits MUST block and route to exception handling
- **And** over-cap adjustments MUST route via non-revenue debit or be out of scope — never silently via S3

**25. Allocation precedence (multi-invoice payment)**
- **Given** a payment that partially covers multiple open invoices
- **When** allocation is computed
- **Then** the order MUST be deterministic (default oldest posting date first, ties by smallest invoice id)
- **And** statutory jurisdiction rules MUST take precedence over tenant overrides and the platform default

**26. Revenue-stream disaggregation**
- **Given** any revenue-affecting journal line
- **When** the line is committed
- **Then** it MUST carry a mandatory revenue-stream classification preserved via accounts/sub-accounts/dimensions (not free text)
- **And** mixed invoices MUST split amounts by stream

### Non-Functional Requirements (Show-Stoppers)

**1. Tenant isolation**
- **Given** any posted journal entry
- **When** balances, reconciliation, export, audit, or inquiry are produced
- **Then** results MUST be tenant-scoped and MUST NOT mix payer tenants in one entry

**2. Tamper evidence**
- **Given** the production posted-journal store
- **When** financial facts are written
- **Then** the store MUST be tamper-evident and verified periodically with alerting on first inconsistency

## 13. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| Rating / Subscriptions | Produces `BillableItem`; recurring/usage aggregates upstream of invoice post | `p1` |
| Payments / PSP | Settlement, allocation, refund, and dispute events; ledger records outcomes | `p1` |
| Tax Engine | Authoritative `TaxBreakdown`; ledger posts, never recomputes tax | `p1` |
| Catalog | `glCode` and tax-category snapshots on posted items | `p1` |
| Contracts | PO/SSP, deferral and recognition precedence inputs | `p1` |
| ERP / GL | Idempotent export consumer / GL of record (Mode A) or mirror (Mode B) | `p2` |
| BSS Architecture Manifest | §1.2.1, §2.1.4, §4.4, §4.5, §8, §9 | `p1` |

## 14. Assumptions

- Upstreams use **at-least-once** delivery with idempotency per business key; ordering is guaranteed only within `(tenantId, aggregateId)` partitions.
- PSP settlement is idempotent by `pspTransactionId`; refunds by `pspRefundId`; conflicting replay payloads hard-error.
- One legal entity per tenant by default; multi-entity tenants require an explicit Design override and per-line legal-entity stamp.
- Tenant CoA / `glCode` mappings are tenant-scoped and supplied via Catalog snapshots at post time.
- Customer-facing transaction history is owned by Invoice/Payment services (SoR), not the ledger.

## 15. Open Questions

| **Question** | **Owner** | **Target Date** | **Answer** | **Date Answered** |
|--------------|-----------|-----------------|------------|-------------------|
| Net vs gross tax presentation (jurisdiction matrix) — **High**: impacts inquiry UI and reconciliation tolerance | PM Team | TBD | — | — |
| SSP source of truth (policy owner and operational source) — **High**: revenue policies on new SKUs | PM Team | TBD | — | — |
| NFR commitments (throughput, p95, recognition window, ERP export SLA, close window) — **Blocking**: engineering capacity planning | PM Team | TBD | — | — |
| FX provider primary + fallback list, stale-rate allowance, tenant override — Medium | PM Team | TBD | — | — |
| Tamper-evidence mechanism selection — Medium: affects DR and verification cadence | PM Team | TBD | — | — |
| BSS vs ERP system-of-record | Architecture Team | 2026-04-29 | Default: BSS is the authoritative billing subledger; ERP/GL is downstream mirror or final-entry. BSS owns export replay and BSS-originated corrections; Finance owns GL-side true-up. Tenant ERP operating mode is a Design/deployment config. | 2026-04-29 |
| Free-form GL / ad-hoc manual journals | Product | 2026-04-29 | No free-form GL vouchers in MVP. Scope is S1–S6, canonical reversals, governed adjustments, suspense, rounding. Full ad-hoc GL voucher support requires a separate PRD/addendum. | 2026-04-29 |
| GDPR right-to-erasure vs 7-year retention | DPO + Security | 2026-04-29 | Immutable financial records retained for the legal/contractual period (typically ≥ 7 years); erasure via PII minimization, tombstoning, pseudonymized references; posted facts and audit-chain integrity not physically deleted. | 2026-04-29 |
| Upstream ordering / replay contracts | Architecture Team | 2026-04-29 | At-least-once delivery with idempotency per business key; ordering within `(tenantId, aggregateId)`; PSP settlement idempotent by `pspTransactionId`, refunds by `pspRefundId`; conflicting replay hard-errors. | 2026-04-29 |
| Tenant rounding-mode override governance | Finance + Design | 2026-04-29 | Banker's rounding default; tenant override only via Finance-approved, tenant-scoped policy version with effective dating, evidence, audit trail, and historical immutability. | 2026-04-29 |
| Inter-tenant settlement / reseller posting | Product + Architecture | 2026-04-29 | Out of scope for this PRD; belongs to a tenant-hierarchy / reseller settlement PRD. This PRD commits only multi-axis line metadata and the no-mixed-payer-tenant invariant. | 2026-04-29 |
| Unbilled receivable / contract asset | PM Team | 2026-04-29 | Out of scope for MVP. This PRD covers invoice-originated AR and Contract liability. Pre-invoice ASC 606 contract assets require a separate PRD/addendum. | 2026-04-29 |

## 16. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| ASC 606 not specified in manifest | Possible recognition/disclosure gaps | This PRD adds finance requirements (non-conflicting); validate with Finance during Design |
| Duplicate scope vs `PRD-billing-module-202601120119` | Divergent ledger semantics across docs | Intentional specialization; mark billing-module ledger section superseded by this PRD for detail |
| `LedgerEntry` naming in Rating vs Billing journal | Naming/federation ambiguity | Design must resolve naming or federation between operational subledger and Billing journal |
| Uncommitted NFR numbers (throughput, p95, close window) | Blocks engineering capacity planning | Commit Open-marked NFRs before Design starts (see §15) |
| Single unapplied bucket without sub-balances | Over-consumption of credit undetected | Require per-event-type running sub-balances; alarm on credit-elected over-consumption |

## 17. Reference Materials

| **Material** | **Link** | **Comments** |
|--------------|----------|--------------|
| BSS Architecture Manifest | `docs/bss/manifest/vz-arch-manifest-bss-only.md` | §1.2.1; §2.1.4; §4.4; §4.5; §8 (PaymentAllocation); §9 (Security, Compliance, Audit) |
| Billing Module (parent scope) | `docs/bss/prd/PRD-billing-module-202601120119/PRD-billing-module-202601120119.md` | AR, ledger, P0 scope |

<!-- migration-note: legacy "Worked examples", "Textual diagrams", "Test strategy", and "Observability — invariant alarms" sections preserved below as illustrative/supporting appendices. They have no direct slot in the kit PRD outline; retained verbatim to avoid content loss. Test obligations and observability detail are normatively owned by the FRs in §6 and by DESIGN. -->

### 17.1 Worked Examples (illustrative)

**Example A — Invoice post (direct split, S1)**: Ex-tax 100 → 40 revenue at post, 60 contract liability; tax 12. DR AR 112; CR revenue 40; CR contract liability 60; CR tax 12.

**Example B — Payment: settlement then allocation (S2)**: Settle 30: DR Cash, CR unallocated 30. Allocate 30: DR unallocated, CR AR 30.

**Example C — FX (illustrative)**: Functional USD, invoice EUR. S1 rate 1.10: DR AR 120 EUR (132.00 USD); CR Revenue 100 EUR (110.00 USD); CR Tax 20 EUR (22.00 USD). S2 settle 1.08 / alloc 1.07; realized FX: DR FX loss 3.60 USD; CR AR 3.60 USD. S6 does not re-lock.

### 17.2 Textual Diagrams

```text
BillableItem ──▶ Invoice (draft) ──▶ Tax ──▶ Issue ──▶ Post ──▶ Ledger JE (AR / Rev / Tax / contract liability)
PaymentSettled ──▶ Ledger JE (Cash / unallocated)
PaymentAllocation ──▶ Ledger JE (unallocated / AR)
Refund (unallocated or on-account) ──▶ Ledger JE (unallocated / Cash)
Refund (after allocation) ──▶ Ledger JE (AR / Cash)
CreditNote ──▶ Ledger JE (contra-revenue and/or contract liability / AR; tax)
Recognition run (scheduled) ──▶ Ledger JE (Contract liability / Revenue)
```

```text
Ledger TB ──▶ Export ──▶ ERP trial balance
     │                    │
     │◀──── variance ─────┘
     └──▶ Revenue Assurance workflow
```

### 17.3 Test Strategy (verification obligations)

| **Obligation** | **Minimum coverage** |
|----------------|----------------------|
| Invariant tests | Zero-sum entries, negative-balance invariants, cumulative credit-note cap, deterministic rounding, recognized-vs-deferred credit-note split. |
| Replay and idempotency tests | Identical replay, conflicting replay, replay after restore, ERP duplicate-key disposition, per-flow idempotency keys. |
| Ordering and exception tests | Allocation before settlement, refund before payment, out-of-order recognition, chargeback phase ordering, queue-vs-quarantine. |
| Reconciliation tests | AR tie-out, Ledger ↔ ERP, Payments ↔ PSP, rounding/FX variance, close-block behavior above tolerance. |
| Audit and lineage tests | Source-document linkage, tenant-scoped retrieval, right-to-erasure tombstone, tamper-evidence verification. |

### 17.4 Observability — Invariant Alarms

| **Alarm category** | **Severity** | **Required behavior** |
|--------------------|--------------|-----------------------|
| Zero-sum violation | Critical | Alert and block further posts in the affected scope until cleared. |
| Negative-balance class violation | Critical (NO-class) / Warn (bounded-YES) | Route to Revenue Assurance per the class table. |
| Recognition double-credit | Critical | Alert Finance Ops and block the affected schedule until cleared. |
| Missing or stale FX snapshot | Critical (missing) / Warn (stale allowed) | Block post when required evidence missing; mark stale snapshots explicitly. |
| Idempotency-key collision | Critical | Reject conflicting payload, capture both payloads in secured audit, alert. |
| Reconciliation variance | Warn at tolerance / Page above | Block period close above tolerance; ticket Revenue Assurance. |
| Failed export with age | Warn → Page on age | Retry with backoff; alert; never drop posted facts. |
| Aged allocation / refund clearing queue | Warn → Page on age | Operator review; release, quarantine, or reconcile per policy. |
| Stage-1 refund without matching stage-2 or reversal | Warn → Page on age | Page Revenue Assurance in addition to the Refund clearing aging alarm. |
| Bill-run partial-failure threshold exceeded | Warn | Pause affected run and require operator review. |
| Tamper-evidence verification failure | Critical | Freeze write path on affected scope; page Audit + Architecture. |
| Clock skew outside window | Warn → Page | Apply clock-skew and timestamp-authority rules. |
| Attempted write-off outside scope | Critical | Reject post, capture actor and intended posting to secured audit, alert Revenue Assurance + Finance Ops. |
