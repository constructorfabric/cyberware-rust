Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Subscriptions — Multi-Tenant Ownership & Transfer (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: AMS (tenant identity, delegation proofs), Policy Engine (transfer gate) | Downstream: Rating/Billing (tenant axes), Analytics (roll-ups) | Owners: BSS Subscriptions team -->

# DESIGN — Multi-Tenant Ownership & Transfer (Slice 7)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-design-tenancy-transfer`

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
  - [4.1 The Three Tenant Axes (normative)](#41-the-three-tenant-axes-normative)
  - [4.2 Delegation Proofs (normative)](#42-delegation-proofs-normative)
  - [4.3 Hierarchy by Reference (normative)](#43-hierarchy-by-reference-normative)
  - [4.4 Ownership Transfer Flow (normative)](#44-ownership-transfer-flow-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice owns the **multi-tenant backbone** every downstream consumer keys on: the three tenant
axes, delegation-proof enforcement for cross-tenant actions, hierarchy strictly **by reference** to
AMS, and the approval-gated **ownership transfer** flow. Subscriptions **references, never invents**
tenant topology — AMS is the identity SoR ([`../PRD.md`](../PRD.md) §6.6). Cross-tenant mutation
without a valid delegation proof is a critical-severity risk, so it is rejected fail-closed with the
proof reference audited.

Transfer is a high-risk transition requiring an `Approval` (slice 01) and emitting the manifest
`OwnershipTransfer*` event trio. No cross-gear seam letter is load-bearing here beyond the AMS
identity boundary and the Policy gate the transfer runs through.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-subscriptions-fr-tenant-axes` | The aggregate carries `resourceTenantId` (operational owner), `payerTenantId` (financial), `sellerTenantId` (channel) — the backbone every consumer keys on (§4.1). |
| `cpt-cf-bss-subscriptions-fr-delegation-proofs` | Cross-tenant admin actions carry an auditable delegation proof; an action without a valid proof is rejected + the audit records the explicit proof reference (§4.2). |
| `cpt-cf-bss-subscriptions-fr-hierarchy-reference` | Roll-ups follow account/OrgTier context from AMS; the gear references the AMS/BSS account binding, never forks topology (§4.3). |
| `cpt-cf-bss-subscriptions-fr-event-producers` (transfer) | Transfer is an `Approval`-gated transition emitting `OwnershipTransferRequested`/`Approved`/`Completed` (manifest §4.11) (§4.4). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-subscriptions-nfr-horizontal-partitioning` | Aggregate store + roll-up read models | Partition by the pinned `orderingTenantId` (SUB-D-06 — stable across transfers); bulk read models for account roll-ups; batch Policy where contractually safe | Load test |
| `cpt-cf-bss-subscriptions-nfr-operational-baselines` | Transfer flow | Approval-gated; state-transition p95 < 500ms per step | Fixtures |

#### Key ADRs

No slice-local ADR; tenant identity is an AMS-SoR reference and transfer follows the manifest §4.11
approval pattern.

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-tech-stack-tnt`

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Presentation | Cross-tenant admin paths + transfer operations behind the gateway; delegation-proof binding | Rust, REST/OpenAPI |
| Application | Axis resolution, delegation-proof enforcement, transfer flow orchestration | Rust module in the `subscriptions` gear |
| Domain | Tenant-axis value objects, delegation-proof reference, transfer state + `Approval` | Rust; GTS + Rust domain structs |
| Infrastructure | Transfer-approval table; roll-up read models | PostgreSQL, SecureORM |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Reference identity, never invent it

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-reference-identity-tnt`

Tenant identity + topology are AMS SoR; Subscriptions references the account binding and never forks
or invents topology ([`../PRD.md`](../PRD.md) §6.6).

#### Cross-tenant mutation is proof-gated

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-proof-gated-tnt`

Every cross-tenant admin action carries an auditable delegation proof or is rejected; the audit
always records the explicit proof reference ([`../PRD.md`](../PRD.md) §6.6 AC 10).

### 2.2 Constraints

#### Transfer is approval-gated

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-transfer-approval-tnt`

Ownership transfer is a high-risk transition requiring an `Approval` (slice 01) and the
`OwnershipTransfer*` event trio; it runs through the Policy gate like any resource-affecting change
([`../PRD.md`](../PRD.md) §6.7, manifest §4.11).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-domain-model-tnt`

- **`TenantAxes`** — `resourceTenantId`, `payerTenantId`, `sellerTenantId` on the aggregate.
- **`DelegationProof`** — the auditable proof reference carried by a cross-tenant action.
- **`OwnershipTransfer`** — the transfer flow state (requested → approved → completed) + its `Approval` (slice 01) and the new tenant binding.

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-tenancy-tnt`

- **`AxisResolver`** — resolves + validates the three axes against AMS references.
- **`DelegationEnforcer`** — rejects a cross-tenant action lacking a valid proof; records the proof reference in audit.
- **`TransferOrchestrator`** — drives the approval-gated transfer flow + the `OwnershipTransfer*` events through the Foundation gate.

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-interface-transfer-tnt`

Cross-tenant admin operations bind a delegation proof; `transfer` is an `Approval`-gated
`TransitionRequest`. The `OwnershipTransfer*` event contract + wire mappings are owned by
[`08-events-billing.md`](./08-events-billing.md) / [`09-consumer-contracts.md`](./09-consumer-contracts.md).

### 3.4 Internal Dependencies

Depends on [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (transfer `TransitionRequest`,
`Approval`, gate). Feeds [`08-events-billing.md`](./08-events-billing.md) (transfer events) and the
roll-up read models consumed by Analytics.

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|---------------------------|----------|
| AMS | Tenant identity + the three axes + delegation-proof backbone | [`../PRD.md`](../PRD.md) §6.6 |
| Policy Engine | Gate for the transfer transition | SEAMS **SUB-E1** |
| Analytics | Consumes roll-up read models + transfer facts | [`../PRD.md`](../PRD.md) §3.2 |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-flow-transfer-tnt`

**Ownership transfer**: `OwnershipTransferRequested` (with delegation proof) → `Approval`
(maker-checker) → `OwnershipTransferApproved` → Policy gate + rebind the tenant axes in one Foundation
commit → `OwnershipTransferCompleted`. A missing/invalid delegation proof rejects at request time with
the attempt audited.

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-storage-tenancy-tnt`

Owned here: `ownership_transfer` (flow state + new binding) using the Foundation `approval`; tenant
axes + delegation-proof references ride the aggregate / transition record. Roll-up read models are
projections. Concrete DDL is Design.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-deployment-tnt`

No slice-specific topology beyond the Foundation's; roll-up read models are projected off the commit
path ([`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) §3.8).

## 4. Additional Context

### 4.1 The Three Tenant Axes (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-axes-tnt`

- The aggregate carries `resourceTenantId` (operational owner of the resources), `payerTenantId` (financial responsibility / consolidated billing), and `sellerTenantId` (channel/marketplace seller) — the multi-tenant backbone every downstream consumer keys on ([`../PRD.md`](../PRD.md) §6.6).
- The ordering/partition key uses the **pinned `orderingTenantId`** — stamped at creation (= `resourceTenantId` at creation) and **immutable** (SUB-D-06): a transfer rebinds the commercial axes, never the ordering key, so pre- and post-transfer events share one partition and no aggregate row set ever migrates partitions (slice 01 §4.2, SEAMS **SUB-R1**).

### 4.2 Delegation Proofs (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-delegation-tnt`

- A cross-tenant admin action MUST carry an **auditable delegation proof** (manifest §2.1.3); an action without a valid delegation is rejected, and the audit record includes the explicit proof reference ([`../PRD.md`](../PRD.md) §6.6 AC 10).

### 4.3 Hierarchy by Reference (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-hierarchy-tnt`

- Commercial roll-ups follow **account** + **OrgTier** context from AMS; Subscriptions **references** the AMS/BSS account binding and MUST NOT invent tenant topology — one identity SoR, no BSS fork ([`../PRD.md`](../PRD.md) §6.6).

### 4.4 Ownership Transfer Flow (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-normative-transfer-tnt`

- Transfer is a high-risk transition: an `Approval` record is required (slice 01) and the flow emits `OwnershipTransferRequested`/`Approved`/`Completed` (manifest §4.11); it passes the Policy gate like any resource-affecting change ([`../PRD.md`](../PRD.md) §6.7).
- The axis rebind is a single Foundation commit **on the pinned partition** (SUB-D-06); `OwnershipTransferCompleted` carries **both the old and the new axes** so consumers (rating, Billing, the check surface) re-key their own projections — there is no stream barrier and no partition migration (AC 26).
- **Guards before side effects (2026-07-15 review fix; ordering corrected 2026-07-19 — F-07-1):** the pure guards run **before** any irreversible side effect. The delegation proof is validated at request time **and re-validated** (approvals can take days; an expired/revoked proof aborts, audited), and a `payerTenantId` rebind re-runs the **overlap check** against the new key (slice 03 §4.4) fail-closed — **both before** the transfer issues any **OSS re-homing work order**. Only once every guard passes does the transfer coordinate the OSS re-homing (Foundation async note); the axis rebind and the tenant-isolated **entitlement check surface re-key** commit on OSS confirmation. This ordering means a guard failure (colliding overlap, expired proof) aborts the transfer with the resources **still homed to the old tenant** — never the earlier hazard of resources re-homed while ownership stayed behind. (Guards that can only be evaluated against post-move state, if any, run as the completing commit's final check, but resource re-homing is never issued ahead of the overlap/proof guards.)
- **Collection boundary:** the payer rebind defaults to **next-cycle** — the in-flight billing period stays with the old payer (the recurring fact carries the **payer in force at the period start** — slice 08 §4.3, re-anchored from cut time by SUB-D-20, 2026-08-01, so even a **late/revival cut running after the transfer** still lands the consumed period on the payer it was consumed under; Billing posts to the fact's frozen payer, the default is enforced by the fact itself, not by posting-time re-resolution); an immediate rebind requires the Billing-side treatment of the mid-period split, which is an open Product/Billing question ([`../PRD.md`](../PRD.md) §15). Cross-currency/region transfers are rejected toward **cancel+new** (slice 02 §4.2, slice 03 §4.3). **Transfer of a nonpayment-suspended subscription (SUB-D-22, 2026-08-01):** legal once the ladder is no longer running (slice 01 §4.2 matrix); the transfer commit **re-resolves the SUB-D-16 dwell deadline** from the acquiring payer's ladder (ladder → tenant → platform), anchored at the original suspension instant — the deadline's source of authority is the payer, so a rebind re-evaluates it (audited); if the re-resolved deadline is already past, the terminal step fires immediately after the commit — the approving transferee accepted an exhausted episode.

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §6.6 (`fr-tenant-axes`, `fr-delegation-proofs`, `fr-hierarchy-reference`), §6.6 AC 10, §6.7 (`OwnershipTransfer*`), §7.1 (partitioning NFR).

**Traces to**: `cpt-cf-bss-subscriptions-fr-tenant-axes`, `cpt-cf-bss-subscriptions-fr-hierarchy-reference`, `cpt-cf-bss-subscriptions-fr-delegation-proofs` *(single-owner FR claims — the P2 traceability convention adopted 2026-08-01, wave-3 review #24h; shared mechanics other slices cite stay narrative, each FR has exactly one owning slice)*
- **Seams**: AMS identity boundary + **SUB-E1** (transfer gate); shares `resourceTenantId` with **SUB-R1** — [`../SEAMS.md`](../SEAMS.md).
- **Slices**: [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (transfer transition, approval, gate), [`08-events-billing.md`](./08-events-billing.md) (transfer events), [`09-consumer-contracts.md`](./09-consumer-contracts.md) (event contract).
