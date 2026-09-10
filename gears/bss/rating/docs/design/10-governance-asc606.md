Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Governance & ASC 606 (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (Product Catalog), Contracts, Finance | Downstream: Pricing (publish pipeline), Billing, Finance | Owners: BSS Rating team -->

# DESIGN — Governance & ASC 606 (Slice 10)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-governance-asc606`

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
  - [4.1 Single Governance Engine (normative)](#41-single-governance-engine-normative)
  - [4.2 The Four Publish-Time Validators (normative)](#42-the-four-publish-time-validators-normative)
  - [4.3 Ledger Separation Rationale (normative)](#43-ledger-separation-rationale-normative)
  - [4.4 ASC 606 Reference Emission (normative)](#44-asc-606-reference-emission-normative)
  - [4.5 Bundle Rev-Share Pass-Through (normative)](#45-bundle-rev-share-pass-through-normative)
  - [4.6 AuthZ Resource and Action Catalog (normative)](#46-authz-resource-and-action-catalog-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This is the one Rating slice that is **publish-time, not hot-path**. Its governance concern
runs inside the *pricing gear's* publish pipeline, not in rating-core: per the resolved seam G1
([`../SEAMS.md`](../SEAMS.md), T-D-06) there is exactly **one** catalog-publish approval engine
— pricing Slice 5's `MaterialityEvaluator` + `ApprovalWorkflow` with its own `approval_policy`
resource and FinanceReviewer approver
([`../../../pricing/docs/design/05-governance.md`](../../../pricing/docs/design/05-governance.md)).
Rating contributes its **four publish-time checks as registered fail-closed validators** in
that pipeline (§4.2) and runs **no second workflow**: no Rating approval state machine, no
approver role, no approval store. The ledger's `dual_control_policy` stays a separate bounded
context — same maker-checker *pattern*, different *policy* (§4.3).

The slice's two evaluation-side concerns are deliberately thin **pass-throughs**: the ASC 606
references `performanceObligationRef` + `sspSnapshotPointer` ride every outcome envelope
(nullable, both null at MVP, immutable once non-null — §4.4), and bundle `sum_of_parts`
evaluation **sums** component lines while reading rev-share only as the publish-normalized
`effective_share_bp` (seam B1, §4.5). Rating never recomputes supplied evidence
([`../PRD.md`](../PRD.md) §14) — it validates at publish, then evaluates over what was
validated.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-publish-approval-governance` | The four checks — ambiguous precedence, ambiguous meter mapping, missing anti-drift cap on material chains, undeclared-dimension contract overlays — register as fail-closed validators in the pricing Slice 5 pipeline (§4.1, §4.2); every run emits auditable events with actor, before/after references, and effective times via the pricing audit trail. |
| `cpt-cf-bss-rating-fr-asc606-traceable-identifiers` | The outcome envelope always carries `performanceObligationRef` + `sspSnapshotPointer` (nullable; both null at MVP); non-null values are immutable once emitted — later catalog changes never alter an emitted reference (§4.4). |
| `cpt-cf-bss-rating-contract-pricing-readmodel` (§9.2 — seam B1 home) | `sum_of_parts` summing at eval is Rating's; rev-share is read exclusively as `effective_share_bp` (normalized at pricing publish, D-07) and passed through untouched (§4.5). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-audit-segregation` | Pricing Slice 5 engine + registered validators | 100% of material publishes carry the single engine's multi-approver sign-off; the Rating validators run fail-closed on every such publish; audit rows commit hash-chained in the same transaction (D-14) — Rating adds no second audit store | Publish-pipeline audit-completeness test |
| `cpt-cf-bss-rating-nfr-resilience` (fail-closed posture) | Validator verdicts | A validator that cannot evaluate its subject fails closed — it blocks the publish rather than guessing; there is no advisory/warn-only mode for the four checks | Publish-pipeline negative fixtures |
| Hot-path NFRs (`throughput-latency`, `horizontal-scale`) | Not allocated here | Publish-time slice: zero hot-path cost; the ASC refs and effective shares are snapshot pass-throughs stamped during ordinary emission | Design |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | The adopt-don't-fork precedent: governance takes the same posture — one engine (pricing Slice 5), Rating registers rules into it rather than forking a workflow (T-D-06). A dedicated governance ADR is planned per the [`../DESIGN.md`](../DESIGN.md) ADR index; until seeded, T-D-06 is the decision of record. |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-gov`

```text
Pricing publish pipeline       MaterialityEvaluator · ApprovalWorkflow · approval_policy
(pricing gear, publish-time)   (FinanceReviewer) — hosts the registered validators
        ▲
        │ registers (fail-closed)
Rating validator set          precedence-ambiguity · meter-mapping-injectivity ·
(this slice, semantics SoR)    anti-drift-cap-presence · undeclared-dimension-overlay
        —
rating-core emission (hot path)       Asc606RefResolver (null at MVP) · RevSharePassThrough
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | The four validator rules (executed by the pricing publish pipeline); envelope contributors at step 9 | Validator rules hosted in the pricing gear's publish pipeline; Rust modules in rating-core for emission |
| Domain | Validator rule specs + failure semantics; ASC ref sourcing/immutability; effective-share pass-through rule | Rust; GTS + Rust domain structs |
| Infrastructure | **None owned** — approval state, `approval_policy`, and the hash-chained `pricing_audit_log` are the pricing gear's (D-14) | Pricing gear storage; Rating persistence for envelopes |

## 2. Principles and Constraints

### 2.1 Design Principles

#### One engine, registered rules

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-one-engine-gov`

Catalog-publish governance is the pricing gear's; Rating contributes rule content, never
workflow machinery. Materiality thresholds, the two-person invariant, TOCTOU content pinning,
and approver scope are pricing Slice 5 semantics that this slice neither extends nor overrides
(seam G1; T-D-06).

#### Enforce at publish, trust at evaluation

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-publish-enforce-gov`

The four checks make bad configurations unpublishable, so the hot path can rely on them as
catalog guarantees (the 01 §4.1/M11 posture); evaluation still fails closed if an invariant is
somehow violated at runtime, but never re-validates per event.

#### Pass through evidence, never re-derive

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-pass-through-gov`

ASC 606 references, `glCode`, and `effective_share_bp` are frozen inputs consumed as-is
([`../PRD.md`](../PRD.md) §14): Rating stamps them onto the envelope and never recomputes,
re-normalizes, or backfills supplied evidence.

### 2.2 Constraints

#### No second workflow, no approval state

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-no-second-workflow-gov`

Rating holds no approval state machine, no approver role, no approval store, and exposes no
approval API ([`../PRD.md`](../PRD.md) §6.12). Contract-level overrides (step 5) are governed
by Contracts, not by this slice (seam G1).

#### Publish-time only

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-publish-time-gov`

The validators execute only inside the pricing gear's publish pipeline; nothing from this slice
runs per evaluation except stamping the pass-through envelope fields. There is no per-event
governance check on the hot path.

#### Emitted references are immutable

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-immutable-refs-gov`

A non-null `performanceObligationRef` / `sspSnapshotPointer`, once emitted, never changes;
subsequent catalog changes MUST NOT alter an emitted reference ([`../PRD.md`](../PRD.md)
§6.12, AC 11). Downstream MAY ignore null fields.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-gov`

- **`ValidatorSpec`** — the identity, subject, check predicate, and failure code of one of the four publish-time checks (§4.2); Rating is the SoR for the rule *semantics*, the pricing pipeline is the execution host.
- **`ValidatorVerdict`** — `pass` or `fail(code, enumerated findings)`; fail blocks the publish; verdicts are audit-payload material (actor, before/after references, effective times), never stored by Rating.
- **`Asc606Refs`** — `performanceObligationRef` + `sspSnapshotPointer` (nullable, both null at MVP) + the `glCode` pass-through; sourced from frozen Catalog/Contracts inputs, stamped at emission (§4.4).
- **`EffectiveRevShare`** — per `(bundle, vendor SKU)`: `effective_share_bp` + `platform_cut_bp`, read from the published read model; guaranteed by pricing publish to sum to exactly 10000 bp (D-07); read-only in Rating (§4.5).
- **Not owned**: approval units, materiality policy, `approval_policy`, audit rows — pricing Slice 5 entities.

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-governance-gov`

- **`PublishValidatorSet`** — the four `ValidatorSpec`s registered into the pricing aggregate fail-closed validation pipeline (execution and failure semantics per §4.1/§4.2).
- **`Asc606RefResolver`** — resolves the ASC refs from the frozen snapshot inputs (null at MVP), stamps them onto the outcome envelope at step 9, and enforces emit-once immutability for non-null values.
- **`RevSharePassThrough`** — attaches the published `effective_share_bp` set to bundle component lineage untouched; the bundle-level summing itself is the ordinary pipeline totaling of component lines (§4.5).

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-validator-registration-gov`

The **validator registration contract** (Rating → pricing publish pipeline): each registered
rule declares its id, subject (the submitted publish unit), verdict shape
(`pass | fail(code, findings)`), and audit payload. Execution points and failure semantics are
the pipeline's, restated normatively in §4.2
([`../../../pricing/docs/design/01-foundation.md`](../../../pricing/docs/design/01-foundation.md)).

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-asc606-envelope-gov`

The **ASC 606 envelope fields** on every resolved outcome (01 §4.4): `performanceObligationRef`,
`sspSnapshotPointer` (both nullable, null at MVP), `glCode`, and — on bundle component lines —
the effective-share lineage. Consumers (Billing/Finance) MAY ignore nulls; recognition is
theirs, not Rating's.

External boundary contracts (the pricing read-model input contract carrying the registration
and pass-through clauses) are owned by [`11-consumer-contracts.md`](./11-consumer-contracts.md).

### 3.4 Internal Dependencies

[`01-foundation.md`](./01-foundation.md) owns the emission envelope the ASC refs and lineage
ride on, and the catalog-guarantee posture the validators underwrite.
[`03-metering-models.md`](./03-metering-models.md) defines the injective `(meter, dimensionKey)`
mapping that validator 2 enforces at publish;
[`04-overlays-precedence.md`](./04-overlays-precedence.md) defines the precedence and
anti-drift-cap semantics behind validators 1 and 3. Bundle component lines are ordinary
per-line runs (slices 02–07); [`11-consumer-contracts.md`](./11-consumer-contracts.md)
formalizes the cross-gear surface.

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Pricing Slice 5 (governance) | The single approval engine: `MaterialityEvaluator`, `ApprovalWorkflow`, `approval_policy` (FinanceReviewer); the hash-chained audit trail (D-14) | [`../../../pricing/docs/design/05-governance.md`](../../../pricing/docs/design/05-governance.md); SEAMS G1 |
| Pricing Foundation (publish path) | The aggregate fail-closed validation pipeline — the registration surface; submit pre-check + commit re-run | [`../../../pricing/docs/design/01-foundation.md`](../../../pricing/docs/design/01-foundation.md) |
| Pricing Slice 8 (bundles) | `sum_of_parts` component `planId` sets; `effective_share_bp` + `platform_cut_bp` normalized at publish per `(bundle, vendor SKU)` onto the typed `residual_absorber_party` (D-07 as re-typed by pricing D-55) | [`../../../pricing/docs/design/08-bundles.md`](../../../pricing/docs/design/08-bundles.md); SEAMS B1 |
| Contracts | Contract-overlay governance (step 5 overrides) stays there; validator 4 checks the published Plan/SKU dimension set | SEAMS G1; [`../PRD.md`](../PRD.md) §17.1 step 5 |
| Finance / Billing | Consume ASC refs and `glCode`; own recognition schedules and journal entries | [`../PRD.md`](../PRD.md) §5.2, §7.2 |

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-publish-gate-gov`

**Publish gate** (runs in the pricing gear):

1. An author submits a catalog publish (any pricing authoring surface).
2. The aggregate fail-closed validation pipeline runs, including the four registered Rating validators; any failure blocks with an enumerated report — no `PlanPublished`, no read-model warm.
3. `MaterialityEvaluator` classifies the change; a material change opens an approval unit; an independent FinanceReviewer approves or rejects (two-person, content-pinned).
4. The publish commit re-runs the pipeline — the Rating validators included — inside the commit transaction; a commit-time failure voids the approval and returns the subject to draft.
5. Audit rows (actor, before/after references, effective times, validator verdicts) commit hash-chained with the mutation into `pricing_audit_log` (D-14); rating-core stores nothing.

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-bundle-sum-passthrough-gov`

**Bundle summing and pass-through** (runs in rating-core):

1. A `sum_of_parts` bundle line arrives; the pinned snapshot carries the component `planId` set and the publish-normalized `effective_share_bp` + `platform_cut_bp`.
2. Each referenced component resolves through the ordinary per-line pipeline (steps 1–9) on its own full key; a component that fails to resolve fails the whole bundle line closed (§4.5).
3. The bundle-level amount is the sum of the component resolved amounts — the eval-time summing is Rating's; the catalog persists only the reference set.
4. Effective shares attach to the component lineage untouched — no re-derivation, no re-normalization; downstream settlement reads only effective shares (D-07).
5. `Asc606RefResolver` stamps the envelope refs (null at MVP) per §4.4.

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-gov`

**None owned.** Approval state, the `approval_policy` resource, and the hash-chained
`pricing_audit_log` are the pricing gear's (D-14; the SEAMS ownership matrix row "Audit trail /
retention — Pricing"); ledger dual-control state is the Ledger's; ASC refs and effective shares
are envelope values persisted by Rating with the outcome. Rating keeps no validator-verdict,
approval, or audit store — consistent with 01 §3.7.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-gov`

Two placements, neither a new deployable: the **four validators execute inside the pricing
gear's publish pipeline** (publish-time; zero rating-core hot-path cost), while `Asc606RefResolver`
and `RevSharePassThrough` are rating-core code in the `rating` gear (01 §3.8). The
packaging of the registered rules (a shared library crate the pricing pipeline links vs a
pricing-side implementation of the jointly-owned rule specs) is an open engineering item to
settle with the pricing team before Design lock; either way the rule semantics SoR is this
slice and the failure semantics are the pipeline's.

## 4. Additional Context

### 4.1 Single Governance Engine (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-single-engine-gov`

- There is **one** catalog-publish approval engine: pricing Slice 5 — `MaterialityEvaluator` + `ApprovalWorkflow` + its own `approval_policy` resource, approver role FinanceReviewer (seam G1, **RESOLVED → single engine**; T-D-06). Rating runs no second workflow and defines no approval semantics — the engine's materiality, two-person, and content-pinning rules are adopted as-is (§2.1).
- Rating's four publish-time checks (§4.2) are **registered fail-closed validators** in the pricing aggregate validation pipeline; they run on every material publish at submit pre-check and again inside the publish-commit transaction.
- **Contract-level overrides** (step 5) are governed by Contracts, not by the pricing engine and not by Rating (seam G1).
- The `cpt-cf-bss-rating-nfr-audit-segregation` threshold (100% of material publishes multi-approved, validators fail-closed, complete before/after audit) is satisfied *inside* the pricing pipeline: validator verdicts and approvals commit as hash-chained audit rows with actor, before/after references, and effective times (D-14).

### 4.2 The Four Publish-Time Validators (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-four-validators-gov`

Identities per [`../PRD.md`](../PRD.md) §6.12; check content per the underlying §17.1 rules:

| # | Validator (id) | Failure code | Checks (fail-closed at publish) | PRD anchor |
|---|-----------|--------------|--------------------------------|------------|
| 1 | Ambiguous precedence (`rating-val-01`) | `RATING_OVERLAY_PRECEDENCE_AMBIGUOUS` | Equal `precedence` among `PriceOverlay`s with overlapping scope within one class is rejected; the runtime class-order + `priceOverlayId` tie-break stays a safety net, never a license to publish ambiguity | §6.12; §17.1 step 4 |
| 2 | Ambiguous meter mapping (`rating-val-02`) | `RATING_METER_MAPPING_AMBIGUOUS` | The `(meter, dimensionKey)` → charge-line mapping MUST be injective per plan revision; a non-injective mapping is a configuration error | §6.12; §17.1 step 3 |
| 3 | Chain-depth cap presence (`rating-val-03`) | `RATING_CHAIN_CAP_MISSING` | An **overlay composition chain** — computed from the submitted publish unit as the set of stackable overlay layers reaching one priced row across the partner → reseller → customer scope classes — of **depth ≥ 2** without a configured `maxCumulativeMarkup` (and no Finance-set default in force) fails publish; a single-layer, sub-threshold overlay MAY warn. *(Renamed from "material multi-link chain" — review #30: "material" was defined nowhere and collided with pricing's unrelated change-materiality; the predicate is structural: chain depth over the submitted unit.)* | §6.12; §17.1 step 4; §15 |
| 4 | Undeclared-dimension overlay (`rating-val-04`) | `RATING_OVERLAY_DIMENSION_UNDECLARED` | A contract overlay MUST NOT introduce metering dimensions absent from the published Plan/SKU revision | §6.12; §17.1 step 5 |

- **Failure semantics**: any failing validator blocks the publish (no `PlanPublished`, no read-model warm); a commit-time failure voids the pending approval; a validator that cannot evaluate its subject fails closed. Findings are enumerated so authoring can remediate in one pass. Ids + failure codes above complete the `ValidatorSpec` shape §3.1 requires (review #32 — the table had prose names only; the codes follow the host pipeline's RFC 9457 convention, since these verdicts surface through pricing's validation report, and align with the runtime counterparts named at [`03-metering-models.md`](./03-metering-models.md) §3/[`04-overlays-precedence.md`](./04-overlays-precedence.md) §3).
- **Open** ([`../PRD.md`](../PRD.md) §15): the default `maxCumulativeMarkup` value and the clamp-vs-hard-fail mode for validator 3's runtime counterpart — the publish-time cap-presence rule above is normative regardless (a Finance-set default, once configured, is a configured cap).

**Problem responses (RFC 9457):** the four codes surface through the **pricing publish
pipeline's validation report envelope** (422, enumerating `violations[]` — pricing design/01
§3.3; this gear still exposes no synchronous API of its own):
`RATING_OVERLAY_PRECEDENCE_AMBIGUOUS` (equal precedence, overlapping scope, one class),
`RATING_METER_MAPPING_AMBIGUOUS` (non-injective `(meter, dimensionKey)` mapping per plan
revision), `RATING_CHAIN_CAP_MISSING` (an overlay composition chain of depth ≥ 2 with no
configured `maxCumulativeMarkup` and no Finance default), `RATING_OVERLAY_DIMENSION_UNDECLARED`
(a contract overlay introducing undeclared metering dimensions).
- **Open (enforcement point, validator 4)**: [`../PRD.md`](../PRD.md) §6.12 registers the check in the pricing pipeline, while [`../SEAMS.md`](../SEAMS.md) G1 keeps contract-override governance with Contracts; whether the same registered rule additionally gates the Contracts-side override publish (vs gating only the catalog side it can see) needs a joint clarification before Design lock. Recorded, not resolved here.

### 4.3 Ledger Separation Rationale (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-ledger-separation-gov`

Reproduced from the [`../SEAMS.md`](../SEAMS.md) governance topology note — three maker-checker
engines share the *pattern*, not the *policy*:

- **Ledger** — `dual_control_policy` + Finance Approver; subject = financial **postings** (journal entries, refunds over threshold, backdating, period reopen, suspense clearing, GL write-off); materiality = amount/entity thresholds + backdating days.
- **Pricing Slice 5** — `MaterialityEvaluator` + `ApprovalWorkflow` + a **separate** `approval_policy` resource; FinanceReviewer; subject = catalog **publish** (price/plan deltas + always-material triggers). Pricing itself cites the ledger `dual_control_policy` as precedent and deliberately keeps its own policy resource.
- **Rating** — folds into pricing Slice 5: same subject (catalog publish), so registering validators there is consolidation, not duplication.

Merging catalog-publish governance into the ledger would be a **category error**: different
subject (commercial configuration vs financial posting), different approver role, different
materiality; a price change becomes a posting only later, via Rating → Billing → Ledger. No
shared platform governance library exists today (`gears/bss/libs/` holds only `coord`).
**Optional post-launch**: extract a shared maker-checker/audit *mechanism* into `libs/` that
pricing, rating, and ledger instantiate each with its own policy — DRY the mechanism, not the
policy. Explicitly not a launch blocker.

### 4.4 ASC 606 Reference Emission (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-asc606-refs-gov`

- Every resolved outcome carries `performanceObligationRef` and `sspSnapshotPointer` on the envelope, **nullable when not applicable** ([`../PRD.md`](../PRD.md) §6.12, AC 11; 01 §4.4).
- **Both resolve null at MVP**: pricing supplies `glCode` now and defers SSP catalog support to Future — no contradiction; the fields exist so the envelope shape never changes when sourcing lands (seam ASC). `glCode` flows now as an ordinary frozen pass-through. **Revisit checkpoint (review #52 — the D-48 "Tax Engine GA" pattern, which this posture lacked):** re-open when Catalog/Contracts ships the `performanceObligationRef`/`sspSnapshotPointer` supplier — at that gate the null-at-MVP posture, the emit-once immutability, and Finance's recognition inputs are re-confirmed together.
- **Immutability**: a non-null reference, once emitted, is immutable; subsequent catalog changes MUST NOT alter an emitted reference. The refs are frozen-input pass-throughs — Catalog/Contracts supply them, Rating consumes and never recomputes ([`../PRD.md`](../PRD.md) §14).
- **Boundary**: Billing/Finance MAY ignore null fields; recognition schedules, revenue allocation, and journal entries are Finance/Billing — explicitly out of scope here ([`../PRD.md`](../PRD.md) §5.2, §7.2).

### 4.5 Bundle Rev-Share Pass-Through (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-revshare-passthrough-gov`

- For a `sum_of_parts` bundle, the **eval-time summing is Rating's**: each referenced component `planId` resolves through the ordinary per-line pipeline, and the bundle-level amount is the sum of the components' **recurring** amounts — **each component's usage charges rate per its own rows and itemize per `invoiceItemization`, never entering the bundle sum** (pricing `inst-bb-sum`, 2026-07-30 review fix L-8, adopted 2026-08-01 — review #4: the unqualified "sum of component resolved amounts" double-counted usage, once inside the sum and once as the component's own usage line; pricing `inst-bc-frequency` correspondingly narrows the frequency-match guarantee to recurring components); the catalog persists only the reference set (seam B1; [`../../../pricing/docs/design/08-bundles.md`](../../../pricing/docs/design/08-bundles.md)).
- **Partial failure fails the bundle**: if any referenced component fails to resolve **a recurring line the sum requires** (no eligible window on its full key at `t`, unsellable market), the whole `sum_of_parts` bundle line **fails closed** — a partial sum is a misprice, never emitted. A **usage-only component** contributes nothing to the recurring sum and is *not* thereby unresolvable (#4): its usage lines rate independently. Component currency/frequency coverage is a pricing publish-time guarantee (pricing design 08) relied on here; a runtime miss is a defensive fail-closed.
- **Open — bundle-level coupon attachment**: whether a coupon can target the bundle total (vs the component lines it decomposes into) is unpinned; until settled with Promotions/pricing, coupons attach to component lines per their ordinary `applyScope` (slice 06) and no bundle-total scope exists.
- **Rev-share arrives normalized**: pricing publish normalizes the residual per **`(bundle, vendor SKU)` group onto the typed `residual_absorber_party`** (pricing D-55, superseding D-07's bundle-level "absorber (default platform)" — review #24; this slice's §3.1 `EffectiveRevShare` was already per `(bundle, vendor SKU)`) so `SUM(effective_share_bp) + platform_cut_bp = 10000` exactly per group; typed authored values stay pricing-side audit material.
- **Pass-through, never re-derivation**: Rating and downstream read **only** effective shares; Rating attaches them to component lineage untouched and never re-normalizes. Monetary cent-level rounding at settlement is a downstream rule (it also lands on the absorber) and is not evaluation math.
- Component lines remain individually addressable in the lineage so settlement can attribute the summed amount per vendor SKU without recomputation; `invoiceItemization` (`aggregate | itemize`) is a published presentation field consumed downstream, not evaluation math.

### 4.6 AuthZ Resource and Action Catalog (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-authz-catalog-gov`

Added by the 2026-07-31 review (#21 — a guideline MUST): the eleven SecureORM stores of slices
12–16 hold per-subscription usage volumes and money amounts — data at least as sensitive as the
sibling pricing gear's, whose `design/05` carries a dedicated catalog — while this set declared
no `PolicyEnforcer`/PDP coverage at all. Per
`docs/toolkit_unified_system/06_authn_authz_secure_orm.md` ("Every sensitive DB access MUST be
covered by a PDP decision (via PolicyEnforcer)") and `guidelines/GTS.md` §12.7–12.8/§14:

**PEP/PDP model (platform-standard, the pricing S5 / ledger precedent)**: every ctx-bearing
service path calls the shared `access_scope` gate (`authz_resolver_sdk::PolicyEnforcer`) with a
`(resource_type, action)` pair from this catalog **before** touching the repository; the
PDP-compiled `AccessScope` is the SQL filter SecureORM binds to `tenant_id` (reads) and the
write-target membership assertion (writes). Stub type-schemas for every label register at gear
init. Deny-by-default both directions; denied attempts are audit-logged (actor, surface, claim
set).

**Resource-type labels** — GTS ids `gts.cf.bss.rating.<noun>.v1~`, all **OUTSIDE**
`gts.cf.resources.*` (usage and rated money are commercially sensitive: the built-in
Reader/Contributor/Owner roles do NOT auto-cover them):

| Label | Object | Actions |
|-------|--------|---------|
| `gts.cf.bss.rating.usage.v1~` | `usage_record` / `usage_dedup` / `usage_quarantine` (slice 12) | `read`; `replay` (quarantine replay — creates billable charges, slice 12 §4.5) |
| `gts.cf.bss.rating.rated_output.v1~` | `rated_output`, `delta_dedup`, `balance_effect_outbox` (slice 15); the `Q` store + attribution (slice 13) | `read` |
| `gts.cf.bss.rating.rerate.v1~` | The administrative re-rate run (slice 08 §4.1 — T-D-21/T-D-24) | `execute` (operator-initiated, enumerated scope, confirm step) |
| `gts.cf.bss.rating.operations.v1~` | `period_tick_ledger`, `cascade_lane` (slice 14); `billing_delivery_outbox`, `period_state` (slice 16); lane/backpressure controls | `read`, `operate` |

**Endpoint/surface → `(resource, action)` mapping**: the gear has **no external synchronous
API** (slices 12/14/16 each say so) — the covered surfaces are the **operator plane**
(quarantine replay, re-rate trigger, lane controls, operational reads) and every internal
ctx-bearing repository path of the eleven stores; **service identities** (the Billing handoff
consumer, the Contracts write-back lane, the usage-collector feed consumer) carry their own
scoped grants under the platform service trust, never a human role.

**Role → permission matrix**: **RatingOperator** — `usage × read/replay`, `operations ×
read/operate`, `rerate × execute` (the re-rate additionally requires the pricing-side
corrective publish to exist — two-person there, D-10/always-material); **Auditor** —
`usage × read`, `rated_output × read` only; no role combines `replay`/`rerate` with nothing —
both are audited money-affecting actions.

Slices 12–16 inherit this catalog (their stores are enumerated above rather than re-declaring
per slice); the tenant-binding rule every §3.7 already states ("partitioned by the pinned
`orderingTenantId`") is enforced through this gate, not beside it.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-asc606-traceable-identifiers`, `cpt-cf-bss-rating-fr-publish-approval-governance`

- **PRD**: §6.12 (both FRs), §7.1 `nfr-audit-segregation`, §9.2 pricing read-model contract (validator registration + B1 pass-through clauses), §5.1 (bundle summing in-scope row), §5.2/§7.2 (recognition exclusions), §14 (evidence consumed, never recomputed), §15 (anti-drift cap open), AC 11, NFR AC 2, §17.1 steps 3–5 (the rules the validators enforce).
- **Seams**: G1 (RESOLVED → single engine; governance topology note), B1 (bundle summing + effective-share pass-through), ASC (null at MVP) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-06 (single engine; ledger separate), T-D-08 (effective rev-share pass-through leg) — [`../DECISIONS.md`](../DECISIONS.md).
- **Pricing design set**: [`../../../pricing/docs/design/05-governance.md`](../../../pricing/docs/design/05-governance.md) (the engine), [`../../../pricing/docs/design/01-foundation.md`](../../../pricing/docs/design/01-foundation.md) (the validation pipeline), [`../../../pricing/docs/design/08-bundles.md`](../../../pricing/docs/design/08-bundles.md) (D-07 normalization).
- **Slices**: [`01-foundation.md`](./01-foundation.md) (emission envelope; catalog-guarantee posture), [`03-metering-models.md`](./03-metering-models.md)/[`04-overlays-precedence.md`](./04-overlays-precedence.md) (the evaluation semantics the validators protect), [`11-consumer-contracts.md`](./11-consumer-contracts.md) (cross-gear contract surface).
