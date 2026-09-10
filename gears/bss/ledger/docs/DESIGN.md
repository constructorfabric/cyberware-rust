Created:  2026-07-08 by Virtuozzo International GmbH
Updated:  2026-07-08 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Billing Ledger — Technical Design (canonical index) -->
<!-- Related: ./PRD.md, ./ADR/, ./design/ | Owners: @vstudzinskyi (BSS Billing Platform team) -->

# Technical Design — Billing Ledger

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
- [5. Traceability](#5-traceability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-design-main`

> **Canonical design entry point and index.** This document is the Billing Ledger's
> top-level technical design and the anchor for spec traceability. The design is authored
> as a **set of slice documents** under [`design/`](./design/) (a shared Foundation plus
> per-slice handler designs); this page is the single index over that set — architecture
> overview, phased slice map, dependency order, cross-cutting normative statements, the
> ADR index, and the traceability surface — and delegates slice-level specifics (schemas,
> sequences, component internals) to the slice documents so they stay the single source of
> truth for their detail.

## 1. Architecture Overview

### 1.1 Architectural Vision

The Billing Ledger is a double-entry accounting engine for the platform's BSS: every
financial event (invoice post, payment settlement, allocation, credit/debit note, refund,
recognition run, FX revaluation) lands as a **balanced journal entry** whose lines never
mutate after commit. The design is organised as a shared **Repository Foundation**
([`design/01-repository-foundation.md`](./design/01-repository-foundation.md)) — the
double-entry posting engine, canonical schema, universal invariants, total lock order,
idempotency contract, money representation, and in-process data-access API — with each
business capability implemented as a **slice handler** that builds balanced lines and posts
*through* the Foundation API. The Foundation owns no domain policy; slices own no schema.
This keeps the correctness-critical core small and auditable while letting each accounting
flow evolve independently.

Ownership is anchored to the **selling legal entity**: books, period close,
`export_target`, and functional currency are properties of a tenant that legally sells, not
of every tenant. The seller predicate is resolved from platform-owned catalogue data (the
AMS `x-gts-traits.owns_billing_books` trait) rather than a ledger-local taxonomy, so adding
or retiring a selling tenant type is a catalogue change, not a ledger change — see
[`cpt-cf-bss-ledger-adr-book-ownership-predicate`](./ADR/0001-cpt-cf-bss-ledger-adr-book-ownership-predicate.md).
The buyer axis (payer / resource line attribution) runs over the same tenant tree but is
deliberately kept separate from book ownership.

Requirements (WHAT/WHY) live in [`PRD.md`](./PRD.md); the "why this way" rationale for the
book-ownership decision is captured as an ADR in [`ADR/`](./ADR/).

### 1.2 Architecture Drivers

Requirements from [`PRD.md`](./PRD.md) that significantly influence the architecture.

#### Functional Drivers

| Requirement                                          | Design Response                                                                                                                                                              |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cpt-cf-bss-ledger-fr-balanced-journal-entries`      | Foundation posting engine rejects any entry whose debits ≠ credits inside the commit transaction; balance is a hard invariant, not a validation step ([`design/01-repository-foundation.md`](./design/01-repository-foundation.md)). |
| `cpt-cf-bss-ledger-fr-posting-immutability`          | `journal_line` rows are append-only; corrections are new reversing entries. Enforced by the Foundation schema + the tamper chain ([`design/02-audit-immutability-observability.md`](./design/02-audit-immutability-observability.md)). |
| `cpt-cf-bss-ledger-fr-reversal-canonical-pattern`    | A single canonical full-reversal pattern (sign-flipped mirror lines referencing the original entry) is provided by the Foundation and reused by every slice.                 |
| `cpt-cf-bss-ledger-fr-account-classes`               | Fixed account-class taxonomy (AR / Revenue / Contract-liability / Tax / Suspense / …) modelled in the Foundation schema; mapping is data, not code.                          |
| `cpt-cf-bss-ledger-fr-invoice-post-direct-split`     | Invoice-post handler splits an invoice into DR AR / CR Revenue + Contract-liability + Tax legs with suspense routing ([`design/01a-invoice-posting.md`](./design/01a-invoice-posting.md)). |
| `cpt-cf-bss-ledger-fr-payment-settlement-vs-allocation` | Settlement and allocation are distinct steps: payment records cash receipt; allocation clears AR (Mode A) ([`design/03-payments-allocation.md`](./design/03-payments-allocation.md)). |
| `cpt-cf-bss-ledger-fr-credit-note-adjustment`        | Credit notes post as governed reversing/adjusting entries with a cumulative cap ([`design/05-adjustments-notes-refunds.md`](./design/05-adjustments-notes-refunds.md)).       |
| `cpt-cf-bss-ledger-fr-refund-balance-first`          | Refund flow consumes wallet/credit balance before cash egress; ordering enforced by the adjustments slice.                                                                   |
| `cpt-cf-bss-ledger-fr-recognition-schedule-controls` | ASC 606 recognition schedules + runs built via `ScheduleBuilder` ([`design/04-asc606-recognition.md`](./design/04-asc606-recognition.md)).                                    |
| `cpt-cf-bss-ledger-fr-asc606-po-identification`      | Performance-obligation identification drives recognition-schedule construction in the recognition slice.                                                                     |

#### NFR Allocation

Non-functional requirements are specified in [`PRD.md`](./PRD.md) §6. The load-bearing ones
map to the architecture as follows; verification detail is per slice.

| NFR theme            | Allocated to                          | Design Response                                                                                                                             |
| -------------------- | ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| Auditability / integrity | Audit-immutability slice + Foundation | Append-only `journal_line`, hash-chained tamper evidence, freeze/secured store from the first production post ([`design/02-audit-immutability-observability.md`](./design/02-audit-immutability-observability.md)). |
| Durability (RPO = 0) | Foundation + `toolkit-db` backend     | Balanced entry + balance-cache mutation commit atomically in one transaction; durable commit before acknowledgement.                       |
| Tenant isolation     | Foundation data-access API            | Every query is bound by the owning (selling) `tenant_id`; the seller/buyer axes are resolved separately and never conflated.               |
| Correctness under concurrency | Foundation                   | A single total lock order across all slices plus idempotency-keyed ingestion make concurrent posting deadlock-free and replay-safe.        |

#### Key ADRs

| ADR ID                                             | Decision Summary                                                                                                                                                                                                                                            |
| -------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cpt-cf-bss-ledger-adr-book-ownership-predicate`   | Only selling entities own billing books. The seller set is resolved from an AMS-owned catalogue trait (`x-gts-traits.owns_billing_books`) read by the ledger's provisioning gate and owner predicate — not a ledger-local tenant-type list — so the taxonomy stays platform-owned and adding/retiring a selling type needs no ledger change. Book ownership (seller axis) is kept separate from payer resolution (buyer axis) over the same tenant tree. |

### 1.3 Architecture Layers

```text
Slice handlers   invoice-posting · payments-allocation · asc606-recognition ·
(domain policy)  adjustments-notes-refunds · fx-multicurrency · reconciliation-export
       │  build balanced lines, call the Foundation API — own no schema
       ▼
Repository        double-entry posting engine · canonical schema · universal invariants ·
Foundation        total lock order · idempotency · money · provisioning · data-access API
(shared engine)   — owns no domain policy
       │
       ▼
Persistence       toolkit-db backend (BIGINT minor-unit money; append-only journal)
Cross-cutting     audit-immutability (tamper chain / freeze) protects every posting flow
```

#### Design set (ordered by implementation phase)

The numeric prefix = **implementation order** (the ratified phasing). It is **deliberately
not** the canonical PRD slice number: slices are numbered by PRD decomposition but built in
dependency order (e.g. ASC 606 recognition is PRD Slice 4 but built in Phase 3 because
adjustments depends on its schedules; reconciliation-export is design Slice 7 but PRD S7 =
ASC 606 Compliance — the two axes never line up; see
[`design/01-repository-foundation.md` §4.1](./design/01-repository-foundation.md#41-naming-glossary-discipline-and-module-alignment)).

| Doc | PRD slice # | Phase | What it is |
|-----|-------------|-------|------------|
| [`design/01-repository-foundation.md`](./design/01-repository-foundation.md) | 1 (+ naming, was slice 8) | 0/1 | **Foundation**: shared engine — journal, balance caches, commit trigger, lock order, idempotency, money, provisioning, the data-access API. Everything posts through it; built first. Also carries the naming/glossary discipline and the three ledger-wide normative statements (§4). |
| [`design/01a-invoice-posting.md`](./design/01a-invoice-posting.md) | 1 | 1 | Invoice-post handler: legs (DR AR / CR Revenue + Contract-liability + Tax), account mapping, suspense routing, AR aging, full reversal. |
| [`design/02-audit-immutability-observability.md`](./design/02-audit-immutability-observability.md) | 6 | starts in 1, completed in 6 | Tamper chain, freeze, secured store, PII/erasure, alarm catalog. Tamper-evidence is mandatory in prod from the first post (launch blocker); PII/erasure/audit-packs land in Phase 6. |
| [`design/03-payments-allocation.md`](./design/03-payments-allocation.md) | 2 | 2 | Settlement, allocation (Mode A), chargebacks/disputes, wallet. First business value: AR is actually cleared. |
| [`design/04-asc606-recognition.md`](./design/04-asc606-recognition.md) | 4 | 3 | Recognition schedules + recognition runs, `ScheduleBuilder`. Built earlier than its canonical number — adjustments depends on its schedules. |
| [`design/05-adjustments-notes-refunds.md`](./design/05-adjustments-notes-refunds.md) | 3 | 4 | Credit/debit notes, refunds, manual governance. Needs the Phase-2 counters and Phase-3 schedules. |
| [`design/06-fx-multicurrency.md`](./design/06-fx-multicurrency.md) | 5 | 5 | Functional currency, realized FX, rate snapshots. A purely additive layer over 01–05. |
| [`design/07-reconciliation-export.md`](./design/07-reconciliation-export.md) | 7 | 6 | Reconciliations, ERP export, period close gate. Only makes sense once all posting flows exist. |

#### Dependency order

```text
01-repository-foundation (shared engine, schema, invariants, data-access API)
    │
    ├─→ 01a-invoice-posting            (Phase 1)
    │       ├─→ 03-payments-allocation (Phase 2)
    │       ├─→ 04-asc606-recognition  (Phase 3)
    │       │       └─→ 05-adjustments-notes-refunds (Phase 4, also needs 03)
    │       └─→ 06-fx-multicurrency    (Phase 5, additive over 01a–05)
    │
    ├─→ 02-audit-immutability-observability (starts Phase 1, completes Phase 6)
    └─→ 07-reconciliation-export       (Phase 6, needs all posting flows)
```

- `01a-invoice-posting` needs only the Foundation: it is the first posting flow.
- `03-payments-allocation` needs invoice-posting — there must be AR to settle/allocate.
- `04-asc606-recognition` needs invoice-posting (schedules derive from posted legs); built **before** adjustments precisely because adjustments depends on its schedules.
- `05-adjustments-notes-refunds` needs **both** the Phase-2 wallet/counters (03) and the Phase-3 recognized/deferred split (04).
- `06-fx-multicurrency` is additive over 01a–05: it activates the Foundation's native functional columns and the multi-currency trigger relaxation.
- `02-audit-immutability-observability` starts in Phase 1 (Mode S tamper chain is a launch blocker) and completes in Phase 6; it protects every posting flow.
- `07-reconciliation-export` needs all posting flows: the period-close gate and cross-system reconciliations only make sense once every flow that posts into a period exists (a minimal OPEN→CLOSED close subset ships in MVP).

## 2. Principles & Constraints

The three ledger-wide normative statements and the naming discipline are authored in the
Foundation design (§4); they are surfaced here as design principles/constraints with stable ids.

### 2.1 Design Principles

#### Foundation owns schema; slices own policy

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-principle-foundation-owns-schema`

No slice defines ledger schema and the Foundation defines no domain flow; slices are handlers that build balanced lines and post through the Foundation API. Normative: [`design/01-repository-foundation.md` §4.2](./design/01-repository-foundation.md#42-foundation-schema-ownership-normative).

#### Post-only, append-only

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-principle-post-only-append-only`

`journal_line` rows are immutable after commit; every correction is a new balanced (reversing) entry, never an in-place edit.

#### Idempotent ingestion

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-principle-idempotent-ingestion`

Every posting call carries an idempotency key; replays are safe and return the original outcome without double-posting.

#### Total lock order

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-principle-total-lock-order`

A single global lock order across all slices makes concurrent posting deadlock-free.

#### Seller ≠ buyer

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-principle-seller-buyer-separation`

Book ownership follows the selling legal entity; payer/resource attribution is a separate hierarchy walk over the same tenant tree. Normative: [`design/01-repository-foundation.md` §4.4](./design/01-repository-foundation.md#44-ledger-ownership-predicate-normative) · ADR `cpt-cf-bss-ledger-adr-book-ownership-predicate`.

#### Tamper-evidence from first production post

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-principle-tamper-evidence`

The audit hash chain is a launch blocker, active from the first production post, not later hardening.

### 2.2 Constraints

#### Naming & glossary discipline

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-constraint-naming-discipline`

Canonical terms are fixed: `journal_entry`/`journal_line` (not `LedgerEntry`); `UNALLOCATED` ≠ `REUSABLE_CREDIT`; `SUSPENSE` = mapping parking only; chargeback holds in `DISPUTE_HOLD`. Normative: [`design/01-repository-foundation.md` §4.1](./design/01-repository-foundation.md#41-naming-glossary-discipline-and-module-alignment).

#### Money is BIGINT minor units (MVP)

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-constraint-bigint-money`

Money is stored as `BIGINT` minor units for MVP (`NUMERIC(38,0)` deferred); one functional currency per selling legal entity.

#### Call-driven ingestion

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-constraint-call-driven-ingestion`

The ledger posts only in response to explicit calls from upstream BSS flows; it does not poll or self-originate entries. Normative: [`design/01-repository-foundation.md` §4.3](./design/01-repository-foundation.md#43-call-driven-ingestion-model-normative).

## 3. Technical Architecture

The technical architecture is specified per slice in the [`design/`](./design/) set, with the
shared substrate in [`design/01-repository-foundation.md`](./design/01-repository-foundation.md).
This section summarises the cross-slice shape and declares the component/sequence ids;
the phased slice map and dependency order are in §1.3.

### 3.1 Domain Model

Core entities live in the Foundation: `journal_entry` (a balanced set of lines with an
idempotency key and period attribution) and `journal_line` (append-only, signed amount,
account class, payer/resource attribution). Balances are maintained as caches keyed by
account and period; accounting periods carry the OPEN→CLOSED close state on the
book-owning selling entity. Full field-level definitions and the naming/glossary discipline
are normative in [`design/01-repository-foundation.md`](./design/01-repository-foundation.md) §4.

### 3.2 Component Model

Components are handlers over the shared Foundation, not independently deployable services.
Each carries a stable `cpt-cf-bss-ledger-component-{slug}` ID; phasing and dependency order
are in §1.3 and the linked slice doc is normative for its internals.

#### Repository Foundation

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-component-repository-foundation`

Shared posting engine: journal, balance caches, commit trigger, total lock order, idempotency, money, provisioning, and the in-process data-access API ([`design/01-repository-foundation.md`](./design/01-repository-foundation.md)).

#### Invoice-posting handler

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-component-invoice-posting`

Builds DR AR / CR Revenue + Contract-liability + Tax legs with account mapping, suspense routing, AR aging, and full reversal ([`design/01a-invoice-posting.md`](./design/01a-invoice-posting.md)).

#### Audit / immutability

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-component-audit-immutability`

Tamper chain, freeze, secured store, PII/erasure, alarm catalogue; cross-cutting over every posting flow ([`design/02-audit-immutability-observability.md`](./design/02-audit-immutability-observability.md)).

#### Payments / allocation

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-component-payments-allocation`

Settlement, allocation (Mode A), chargebacks/disputes, wallet ([`design/03-payments-allocation.md`](./design/03-payments-allocation.md)).

#### ASC 606 recognition

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-component-recognition`

Recognition schedules + runs via `ScheduleBuilder` ([`design/04-asc606-recognition.md`](./design/04-asc606-recognition.md)).

#### Adjustments

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-component-adjustments`

Credit/debit notes, refunds, manual governance ([`design/05-adjustments-notes-refunds.md`](./design/05-adjustments-notes-refunds.md)).

#### FX / multicurrency

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-component-fx-multicurrency`

Functional currency, realized FX, rate snapshots; additive over the posting core ([`design/06-fx-multicurrency.md`](./design/06-fx-multicurrency.md)).

#### Reconciliation / export

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-component-reconciliation-export`

Reconciliations, ERP export, period-close gate ([`design/07-reconciliation-export.md`](./design/07-reconciliation-export.md)).

### 3.3 API Contracts

The ledger's primary contract is the **in-process data-access API** exposed by the
Foundation (build-balanced-lines → post → commit), consumed by the slice handlers; it is
specified in [`design/01-repository-foundation.md`](./design/01-repository-foundation.md).
Outward-facing surfaces (ERP export, reconciliation) are defined in
[`design/07-reconciliation-export.md`](./design/07-reconciliation-export.md).

### 3.4 Internal Dependencies

- **`toolkit-db`** — transactional persistence backend for the journal, balances, and idempotency records.
- **Coordination lease library** — singleton coordination for period-close / recognition-run background work.

### 3.5 External Dependencies

The ledger integrates with platform and BSS actors defined in [`PRD.md`](./PRD.md): the AMS /
catalogue (seller-set trait and `tenant ↔ commercial-account ↔ legal-entity` mapping), the
billing-orchestration caller (invoice posting), the payments PSP (settlement), the tax
engine (tax legs), the recognition run (ASC 606 schedules), and the downstream ERP/GL
(export target on the selling legal entity). These are integration boundaries, not
components owned by the ledger.

### 3.6 Interactions & Sequences

Per-flow sequences are specified in the corresponding slice documents under
[`design/`](./design/); the dependency ordering across flows is in §1.3. The load-bearing
sequences:

#### Invoice post → balanced legs

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-seq-invoice-post`

Invoice ingestion splits into DR AR / CR Revenue + Contract-liability + Tax legs, routed through the Foundation commit ([`design/01a-invoice-posting.md`](./design/01a-invoice-posting.md)).

#### Payment → settlement → allocation

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-seq-payment-settlement-allocation`

Payment records cash receipt (settlement); allocation subsequently clears AR (Mode A) ([`design/03-payments-allocation.md`](./design/03-payments-allocation.md)).

#### Period close → ERP export

- [ ] `p1` - **ID**: `cpt-cf-bss-ledger-seq-period-close-export`

The period-close gate on the book-owning seller precedes reconciliation and ERP export ([`design/07-reconciliation-export.md`](./design/07-reconciliation-export.md)).

### 3.7 Database schemas & tables

The canonical schema — `journal_entry`, `journal_line`, balance caches, accounting periods,
idempotency records, and per-slice tables — is owned by the Foundation and specified
normatively in [`design/01-repository-foundation.md`](./design/01-repository-foundation.md)
§4.2 (Foundation schema ownership). Slice-specific tables are introduced by their respective
slice documents. Money columns are `BIGINT` minor units for MVP.

### 3.8 Deployment Topology

The ledger runs as a stateless posting service over a shared `toolkit-db` backend;
background work (period close, recognition runs) is coordinated as a singleton via the
coordination lease library. Deployment specifics are platform-standard for a BSS gear.

## 4. Additional context

- **Telemetry** — posting throughput, balance-cache lag, and close-gate state are surfaced per the audit/observability slice ([`design/02-audit-immutability-observability.md`](./design/02-audit-immutability-observability.md)).
- **Risks** — the book-ownership predicate depends on an AMS catalogue change landing (`x-gts-traits.owns_billing_books`); until then the ledger evaluates the interim seller set (`platform` + `partner`), as recorded in [`cpt-cf-bss-ledger-adr-book-ownership-predicate`](./ADR/0001-cpt-cf-bss-ledger-adr-book-ownership-predicate.md).
- **Deferred to future scope (post-MVP)** — cross-currency conversion (rejected in MVP — payments-allocation rejects `ALLOCATION_CURRENCY_MISMATCH`; the conversion-event mechanism is a deferred extension of fx-multicurrency), the statutory allocation registry, contract assets / unbilled, bad-debt / write-off / recovery, the full variable-consideration mechanism, escheatment filing, free-form GL, inter-tenant settlement / reseller payout, `NUMERIC(38,0)` money (`BIGINT` minor units confirmed for MVP), the ledger-side payer re-validation guard against the tenant tree, and historical / as-of temporal balance (reconstructable from `journal_line`). Each slice carries its own deferred markers; the consolidated registry is in [`PRD.md`](./PRD.md) § "Deferred to future scope".

## 5. Traceability

- **PRD**: [`PRD.md`](./PRD.md)
- **ADRs**: [`ADR/`](./ADR/) — `cpt-cf-bss-ledger-adr-book-ownership-predicate`
- **Design set**: [`design/`](./design/) — foundation + per-slice designs; the phased map and dependency order are in §1.3.
