Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Plan & Price Modeling — Catalog Foundation (shared publish engine) (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md | Upstream: Product & SKU registry, Effective-dating PriceWindows | Downstream: Tariffs, Subscriptions, Rating, Billing, Marketplace | Owners: BSS Product Catalog team -->

# DESIGN — Catalog Foundation (shared publish engine) (Slice 1)

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-design-foundation`

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
  - [4.1 Canonical Scope Key (normative)](#41-canonical-scope-key-normative)
  - [4.2 Publish-Through-The-Engine Contract (normative)](#42-publish-through-the-engine-contract-normative)
  - [4.3 Immutability and Change Mechanisms (normative)](#43-immutability-and-change-mechanisms-normative)
  - [4.4 Read Model and pricingSnapshotRef (normative)](#44-read-model-and-pricingsnapshotref-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

The Catalog Foundation is the shared publish engine that every Plan & Price Modeling
capability builds on. It owns the `Plan`/`Price` entity model, the **canonical scope key**,
the draft→publish state machine, the **fail-closed validation pipeline** framework,
append-only published-row history with versioning/supersession, the **read-model projection**
and `pricingSnapshotRef` stamping, and the **frozen event fan-out** plus the
`CatalogVersion`-increment request to the registry. It owns **no capability policy**: what a
billing cycle means, how a tier band validates, what a bundle is — all live in a handler slice
(plan-definition, price-structure, and the rest), each of which authors draft state, registers
its validation rules and its read-model fields, and **publishes through** this Foundation's
API under the invariants defined here.

The catalog's contract is the mirror image of the sibling Billing Ledger's *post through the
engine*: it is **publish through the engine**. Every state change reaches production one way —
author draft → run the aggregate fail-closed validation pipeline → freeze a complete read
model + `pricingSnapshotRef` → emit the frozen event set → request a `CatalogVersion`. There
is no side door that mutates published state, no consumer that reads draft, and no default
substituted for an absent field (absence must have failed publish). This keeps the
correctness-critical publish/immutability/determinism core small and auditable while letting
each pricing capability evolve independently ([`../PRD.md`](../PRD.md) §1.1, §2).

The gear is **one deployable modular monolith** (`pricing`) running in two roles — a
synchronous authoring/publish/preview API and a read-model service — over one PostgreSQL
backend. The authoring path is transactional — draft mutation, validation, publish-commit, and event
enqueue commit atomically **at the post-approval publish step** (the Slice 5 approval gate for
material changes sits between the validation pre-check and that commit); the read path is
served from a projected read model for the p95 < 100ms target and **fails closed** (never
stale) on read-model outage.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-pricing-fr-publish-validation-failclosed` | A single **aggregate fail-closed validation pipeline** runs at publish; slices register rules keyed to the §17.4 rule set; any invalid condition blocks the publish transaction (no `PlanPublished`, no read-model warm). The validation report enumerates every failure so authoring can remediate. |
| `cpt-cf-bss-pricing-fr-published-rows-append-only` | `pricing_price` rows in a published state are append-only: `REVOKE UPDATE, DELETE` from the app role + `BEFORE UPDATE OR DELETE` triggers that RAISE, with a **column whitelist** for the two sanctioned transitions (state-machine `lifecycle_state` flips; monotonic `grandfather_until` tightening — §3.7/§4.3); only never-published `draft` rows are deletable. There is no deletion event to fan out. |
| `cpt-cf-bss-pricing-fr-plan-versioning` | A price/tier change versions the `Plan` and writes **new** immutable `pricing_price` rows; prior rows are retained as history; bound subscriptions continue on their frozen snapshot until renewal or migration. |
| `cpt-cf-bss-pricing-fr-supersession` | Supersession is versioning scoped to **one canonical scope key**: a new immutable row plus opening/closing the corresponding `PriceWindow` (never an in-place mutate, never an overlap), within one `priceEligibility` class and one `chargeKind`. |
| `cpt-cf-bss-pricing-fr-pricing-snapshot` | Publish stamps the catalog-side identifiers sufficient for the manifest `pricingSnapshotRef` (resolved price ids + evaluation-policy version + the **pending** version ref, finalized to the committed `CatalogVersion` on `CatalogVersionPublished`); posted periods never re-query mutable rows; the catalog-side view MUST NOT diverge from the Tariffs composition SoR. |
| `cpt-cf-bss-pricing-fr-consumer-readmodel-resolution` | The projected read model is **monotonic per `CatalogVersion`** (a version is **pin-eligible** only once `CatalogVersionPublished` has fired, **every** subject row it projects is warm-complete — D-101 — **and every earlier version is itself pin-eligible** — D-114: pin-eligibility is a prefix-closed frontier). **Both quantifiers range over this tenant's refs (D-164, §4.4):** `CatalogVersion` is a cross-tenant sequence, so a tenant's committed versions are a sparse subset of it, and read globally neither "every subject row" nor "every earlier version" is a set this tenant can ever satisfy. Consumers resolve exact published values with no draft read and no default substitution; a rating run pins one pin-eligible version and the pin never lags the newest pin-eligible version by > 5s. |
| `cpt-cf-bss-pricing-fr-catalogversion-increment` | On every `PlanPublished` the Foundation requests addressability; the registry is the **sole** incrementer and MAY batch approved publishes; `PlanPublished` carries a **pending** ref and the snapshot pins the committed version on `CatalogVersionPublished`. |
| `cpt-cf-bss-pricing-fr-publish-fanout-atomicity` | Post-commit read-model warming retries to the 5s SLO or marks the publish degraded (`PlanPublishDegraded`); no state exposes a rateable-but-incomplete plan; the pre-commit batching delay is governed by the max batching-delay SLO, not by degraded handling. **That separation is only measurable because the ref row records when this gear learned the version had committed (`commit_observed_at` — D-166, §3.7/§4.4):** from `requested_at` alone the 5s clock measures the batching delay, and every publish behaving exactly as D-47 budgets is marked degraded. |
| `cpt-cf-bss-pricing-fr-event-contract` | A **frozen event-name set** (`PlanCreated`, `PlanUpdated`, `PlanPublished`, `PlanRetired`, and conditionally `PlanMigrationScheduled`, `PlanPublishDegraded`, `BundleUpdated`, `PriceCreated`, `PriceUpdated`, plus the manifest `PriceWindowScheduled`/`Activated`/`Expired`/`Cancelled` — produced by this gear since the window consolidation, D-03 — and **`PriceOverlayPublished`, the fourteenth name (D-248, 2026-08-07)**, whose aggregate is the **overlay id** rather than a plan: an overlay is tenant-scoped and may target no plan at all, so there is no plan stream to order it within. The set is frozen in two places that must agree — this row and `chk_pricing_outbox_event_name` — so a name joins both or neither) emitted from a transactional outbox, ordered per `(tenantId, aggregateId)`, at-least-once, carrying correlation/idempotency keys. |
| `cpt-cf-bss-pricing-fr-price-amount-validation` | Amount ≥ 0, valid ISO 4217, precision = the currency's ISO 4217 minor unit; a missing `(currency, region)` row fails closed (no implicit FX). |
| `cpt-cf-bss-pricing-fr-mutation-idempotency` | Plan/Price create/update accept a client idempotency key; a duplicate returns the original outcome without a second mutation. |
| `cpt-cf-bss-pricing-fr-concurrent-edit` | Optimistic concurrency (ETag/row version) rejects a stale submit and a bulk-vs-interactive collision with a conflict; neither change is silently overwritten. |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| Publish → read-model propagation (p95 ≤ 5s) | Publish engine + outbox + read-model warmer | Batched `CatalogVersion` commit; retry-to-SLO warm or `PlanPublishDegraded`; pin never lags newest completed by > 5s. **The 5s clock starts at `CatalogVersionPublished`, and until D-166 no store in this gear recorded that instant** — `committed_at` is stamped by the finalize, which never runs on the failing path, so the one measurable duration was `requested_at → now`, i.e. the batching delay | Load test on the publish→warm path; batching-delay SLO **ratified (D-47: p95 ≤ 60s, max 5 min; interactive ≤ 5s)** ([`../PRD.md`](../PRD.md) §15). In-gear measurement is against `commit_observed_at` (D-166), an upper bound lagging the true commit by at most one sweep tick; the registry supplying its own commit instant is the accurate form and is the owed cross-gear half |
| Read / preview latency (p95 < 100ms per tenant partition) | Read-model projection store | Single indexed, version-pinned read; no evaluation on the read path | APM on read APIs |
| Determinism / reproducibility | Snapshot + append-only history | Complete frozen `pricingSnapshotRef`, monotonic per version, append-only rows | Design + integration test (later-version publish does not alter a prior snapshot) |
| Read-model availability / DR RPO-RTO | Read-model store + topology | Fail-closed on outage (never stale); 99.9% / RPO 5m / RTO 30m | Committed — ratified 2026-07-28 ([`../PRD.md`](../PRD.md) §14) |
| Idempotency-key TTL; plan/tier size caps | Publish engine | Idempotency-dedup store (TTL **24h**, evaluated **at claim time** and **before** the payload-digest comparison — **D-142**, 2026-08-02, found while building the draft-authoring plane: compared the other way round, the first payload to touch a key owns it forever, since nothing deletes a dedup row, and the ratified 24h is then not a bound at all; §3.7 carries the row's two states); publish-time size validation, of **two different kinds** (2026-08-03, D-160 — this cell had listed all four caps as one item): the **soft** caps (100 bands/row, 500 rows/plan) are an **advisory** that never blocks, reported as `PLAN_SIZE_SOFT_CAP_EXCEEDED` (§3.3) in the report's `warnings[]` channel, while the **interval** caps (366d/24m) are hard and fail publish (`INVALID_CUSTOM_INTERVAL`); all four are per-tenant in `pricing_policy_object` (D-152) | Committed — ratified 2026-07-28 |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-pricing-adr-canonical-scope-key` | The single scope key is `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)` — the manifest key extended additively so hybrid components and a grandfathered row + its successor are distinct keys with concurrent active windows. |
| `cpt-cf-bss-pricing-adr-grandfathering-cohort-axis` | Multi-generation grandfathering: the additive `cohort` axis (= the cutover instant; `none` on non-grandfathered rows) makes every cutover a **new** generation on its own key; within the grandfathered class Tariffs selects by the cohort of the subscription's pinned price id. |
| `cpt-cf-bss-pricing-adr-pricewindow-consolidation` | The `PriceWindow` machinery is gear-owned (Slice 7): store, state machine, activation job, `PriceWindow*` production; multi-window units are local ACID transactions. |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-pricing-tech-stack`

```text
Capability slices (modules)   plan-definition · price-structure · currency-tax · governance ·
                              consumer-contracts · pricewindow-linkage · bundles · price-overlays ·
                              advanced-primitives · lifecycle · operator-efficiency
        │  (publish API: authorDraft / validate / publish / projectReadModel / requestVersion)
        ▼
Publish Engine (Foundation)   ScopeKey · DraftStateMachine · ValidationPipeline ·
                              VersioningStore · ReadModelProjector · SnapshotStamper · EventOutbox
        │
        ▼
PostgreSQL                    plan / price (truth + append-only history) · read_model (projection) ·
                              catalog_version_ref · outbox · policy objects · audit store
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Presentation | REST authoring/publish/preview + read-model surfaces behind the inbound gateway; RFC 9457 problems; OAuth 2.0; ETag optimistic concurrency | Rust, REST/OpenAPI, inbound API gateway |
| Application | Capability slices author draft state and register rules/read-model fields | Rust modules in the `pricing` monolith |
| Domain | The publish engine, canonical scope key, validation pipeline, versioning/immutability, snapshot contract | Rust; GTS + Rust domain structs |
| Infrastructure | Append-only history, projected read model, audit store, transactional outbox | PostgreSQL (single primary + replicas), SecureORM |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Foundation owns publish; slices own capability policy

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-principle-foundation-owns-publish-fnd`

No slice defines the scope key, emits an event, or stamps a snapshot; the Foundation defines
no capability semantics. Slices author draft state, register validation rules and read-model
fields, and publish through the Foundation API. Normative: [§4.2](#42-publish-through-the-engine-contract-normative).

#### Fail closed, always

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-principle-fail-closed-fnd`

Any invalid or ambiguous condition blocks the publish transaction; the absence of a required
field is a publish failure, never a downstream default. Consumers never read draft state.

#### Published state is append-only

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-principle-append-only-fnd`

Published `pricing_price` rows are immutable history; change is a new immutable row via
versioning/supersession + `PriceWindow`. Only never-published `draft` rows are deletable.
Normative: [§4.3](#43-immutability-and-change-mechanisms-normative).

### 2.2 Constraints

#### Money is ISO 4217 minor units

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-constraint-money-fnd`

Amounts are stored as integer minor units at the currency's ISO 4217 scale (0 for JPY/KRW, 2
default, 3 for BHD/KWD/OMR); a flat 2-decimal cap MUST NOT be assumed; amounts are `≥ 0`
(a negative amount is rejected — `AMOUNT_NEGATIVE`); a code that is not valid ISO 4217 is
rejected (`CURRENCY_INVALID`); no implicit FX — a missing `(currency, region)` row fails closed.

#### Author-driven mutation; UTC time

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-constraint-author-driven-fnd`

The catalog mutates state only in response to explicit authoring/publish/lifecycle calls; it
does not self-originate rows. All effective dating, window boundaries, `grandfatherUntil`,
`availableFrom`/`availableTo`, and anchor math are UTC.

**UTC at millisecond resolution (normative, D-144, 2026-08-02, found while building the
draft-authoring plane).** Every instant this gear **authors, carries in a contract field,
publishes or compares** is quantized to the millisecond: cutover instants and the `cohort` axis
derived from them (§4.1), window `effectiveFrom`/`effectiveTo`, `grandfatherUntil`,
`availableFrom`/`availableTo`, and the D-88 changeover instant. An authored instant carrying
finer precision **fails validation** (`TIMESTAMP_PRECISION_EXCEEDED`, 422) rather than being
truncated. The line above fixed the zone and left the resolution open, which is not a spelling
gap: `cohort` is a scope-key axis matched for **equality**, and D-126's bootstrap case compares
it against a window `effectiveTo` produced by a different code path in a different gear — two
instants denoting the same moment at different resolutions are not equal, so an unquantized axis
makes a generation unfindable by exactly the subscribers grandfathering exists to protect.
Rejected: **truncating at the boundary**, which is what an unstated quantum degenerates into. It
silently moves the instant a scope-key axis, a window bound and an approval-time floor are all
derived from, and a truncating producer agrees with a non-truncating consumer until the day they
do not, with no failure in between — the same posture the money-side sibling `PRECISION_EXCEEDED`
(§3.3) already takes, for the same reason. Storage bookkeeping no operator authors — `created_at`,
audit-chain and outbox timestamps — is outside the rule: it enters no contract field and is
compared with nothing. The published field set is unchanged; **joint with Rating**, the owed
D-126 cohort-bootstrap adoption gains one clause, that the comparison is at this quantum.

#### AuthZ gate before the repository

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-constraint-authz-gate-fnd`

Every ctx-bearing service path calls the shared PEP `access_scope` gate
(`authz_resolver_sdk`) with its catalogued `(resource_type, action)` pair **before** touching
the repository; the PDP-compiled `AccessScope` is the SQL filter SecureORM binds (reads) and
the write-target membership assertion (writes). The resource/action catalog, the
endpoint mapping, and the role matrix are normative in the governance slice
([`05-governance.md`](./05-governance.md) §AuthZ Resource and Action Catalog); labels are
GTS ids `gts.cf.bss.pricing.<noun>.v1~` outside `gts.cf.resources.*`, registered as stub
type-schemas at gear init.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-domain-model-fnd`

The Foundation owns four core aggregates; capability slices extend them with their own fields
and child tables (phases, add-on rules, bundles, price overlays) without redefining the core.

- **`Plan`** — binds a **published** `skuId` to a billing cycle, a mandatory `PlanTier`, optional plan phases and composition rules, and optional `availableFrom`/`availableTo` purchasability dates. Carries a lifecycle state per revision row (`draft` → `published` → `superseded` | `retired`, plus the terminal `abandoned` a discarded draft revision takes — D-145; the `superseded` flip is D-90's plan-revision analogue of the price rows' flip-at-commit) and a `revision` that is an **identity**, minted `max + 1` and never re-minted (D-145 — §4.3). Capability meaning of the cycle/composition fields is owned by the plan-definition slice.
- **`Price`** — a price row on the **canonical scope key** (§4.1) with an amount (ISO 4217 minor units), `modelKind`, tier bands, evaluation-policy fields, `taxInclusive`, lifecycle metadata (`priceEligibility`, optional `grandfatherUntil`), and a supersession pointer to the row it replaces within its scope key. Published rows are append-only; a prior row is retained as history.
- **`ReadModel`** — the projected, per-`CatalogVersion` frozen view a consumer resolves: `{skuId, planId, priceId}`, model kind, ordered tier bands, evaluation-policy fields, phase→price map, **phase→grant-set map** (per-phase entitlement grant set when authored — D-41; else the plan-level `PlanTier`-driven grant set), billing descriptors, and the consumer contracts (proration/plan-change/entitlement) contributed by their slices. Monotonic per version; never reflects draft state.
- **`pricingSnapshotRef`** — the composite reference (resolved price ids + evaluation-policy version + version ref) whose catalog-side identifiers are stamped at publish with a **pending** version ref and **finalized** to the committed `CatalogVersion` on `CatalogVersionPublished`, immutable thereafter; pinned by consumers (composition SoR: Tariffs).

Supporting Foundation objects: the **tenant policy objects** (approval-threshold policy,
tax-display policy — both fail-safe), the **idempotency-dedup** store, the **transactional
outbox**, and the **audit store** (append-only, actor/before-after/approval trail).

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-component-foundation-fnd`

The Foundation is a set of in-process components behind one publish API:

- **`ScopeKey`** — constructs and validates the canonical scope key, applies the axis defaults **D-19** types (`priceOverlay = base`, `phase =` the plan's terminal `phase_id`, `priceEligibility = all_subscriptions`, `cohort = none`), and backs the row-uniqueness index.
- **`DraftStateMachine`** — the `draft` → `published` → `superseded` | `retired` transitions (per revision row — D-90), plus the terminal `draft → abandoned` flip a discarded plan draft revision takes (**D-145**, 2026-08-02 — [`02-plan-definition.md`](./02-plan-definition.md) `inst-pl-abandon`) **and, since D-231 (2026-08-07), the one a discarded price-overlay draft revision takes for the same reason** ([`09-price-overlays.md`](./09-price-overlays.md) §6). Only `draft` rows are mutable; only never-published `draft` **price** rows are deletable — a plan's open draft revision row is abandoned rather than deleted, so the `revision` number it consumed is never re-minted (§4.3) — **and an overlay's is not deletable either since D-231**, which closes the one place the rule did not hold and where a re-minted number let a stale `If-Match` match a different revision under the same identity.
- **`ValidationPipeline`** — runs the aggregate fail-closed rule set at publish; slices register rules; a single failure blocks the publish transaction and populates the validation report.
- **`VersioningStore`** — writes new immutable rows on versioning/supersession, retains history, and enforces append-only via role + triggers.
- **`ReadModelProjector`** — materialises the frozen per-version read model and drives warm-completion; fails closed on outage.
- **`SnapshotStamper`** — stamps the catalog-side identifiers sufficient for the manifest `pricingSnapshotRef` (composition SoR: Tariffs) and requests the `CatalogVersion` increment from the registry.
- **`EventOutbox`** — emits the frozen event set transactionally, ordered per `(tenantId, aggregateId)`, at-least-once with dedup keys.

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-interface-authoring-publish-fnd`

The **authoring + publish** contract (`cpt-cf-bss-pricing-interface-authoring-publish`):
create/update/clone plans and price rows in `draft`, run fail-closed validation, submit for
approval (two-person rule for material changes), and publish — emitting the frozen event set
and requesting a `CatalogVersion`. Accepts client idempotency keys; enforces optimistic
concurrency via ETag/row version.

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-interface-catalog-read-model-fnd`

The **published read model** contract (`cpt-cf-bss-pricing-interface-catalog-read-model`):
per committed `CatalogVersion`, the complete plan/price read model resolvable via
`pricingSnapshotRef`, monotonic per version, no draft reads, additive-only within a major
version. It additionally publishes the **pin frontier** —
`GET /bss-pricing/v1/catalog-version/frontier` → the tenant's current pin-eligible
`CatalogVersion` + its `advanced_at` (`plan × read`, service identity; D-136, §4.4) — because
pin-eligibility is a version-level, prefix-closed predicate no consumer can evaluate for
itself: this is the value a consumer pins at the start of a resolution/rating run, and the
referent of the ≤ 5s pin-lag rule.

The base-price **preview** (`cpt-cf-bss-pricing-interface-price-preview`) and the external
integration contracts ([`../PRD.md`](../PRD.md) §9.2) are refined in the slices that own their
payloads. Concrete schemas, proto, and **slice-specific** error taxonomies are owned by these
slice designs. Failure modes of the engine itself carry **Foundation-owned** RFC 9457 problem
types, referenced (never redefined) by slices. **Problem responses (RFC 9457):**
`DUPLICATE_SCOPE_KEY` (409 — canonical
scope-key uniqueness), `STALE_VERSION` (409 — ETag/row-version conflict),
`IDEMPOTENCY_PAYLOAD_MISMATCH` (409 — same key, different payload),
`IDEMPOTENCY_KEY_IN_FLIGHT` (409 — this key is claimed and not yet answered; retry, and you
will receive either the stored response or `IDEMPOTENCY_PAYLOAD_MISMATCH`. D-143, 2026-08-02,
found while building the draft-authoring plane, **flagged for veto** — the case arises with no
contract violation by anyone (§3.7), so stating the one-transaction rule normatively cannot
close it, and the surface layer had nothing to answer a client with),
`OPEN_DRAFT_REVISION_EXISTS` (409, naming the open revision — a second draft revision on a plan
that already holds one. D-146, 2026-08-02: a **narrowing** of `LIFECYCLE_FORBIDDEN`, not an
addition beside it, so no refusal ends up with two codes; it is a uniqueness conflict on the
`(plan_id) WHERE lifecycle_state = 'draft'` partial index (§3.7) rather than a state-machine
transition, and the operator's next action is a real one — go and edit that revision — §4.3.
*Whose call reaches it, stated 2026-08-03 because the index sentence above is easy to read as a
formality:* under D-170 no authoring request **asks** for a second draft — a `PATCH` resolves the
plan's editable revision for itself — so the caller this refusal answers is the **loser of the
race for that slot**, whose sibling call created the draft between this one's resolution and its
insert. That is exactly the index conflict this gloss names, and it is why the refusal is not
`CONCURRENT_MUTATION`: the loser's remedy is not to replay the call as sent but to re-read and
edit the draft that now exists, which is the "different and available action" §4.3 turns on),
`PLAN_RETIRED_NO_SUCCESSOR` (422 — opening a revision on, or re-publishing, a retired plan.
D-146: the second narrowing, **replacing** the re-publishing clause of the gloss below, because
a retired plan can never publish again and the operator has no next action at all — §4.3),
`PLAN_ABANDONED_NO_SUCCESSOR` (422 — an authoring call naming a plan whose every revision is
`abandoned`: it holds no current revision and no open draft and can acquire neither, so no
further revision — first or successor — will ever open on it, and the plan id is spent.
**D-145 as amended 2026-08-02**: that rule's "a replacement draft opens immediately" holds for
every plan that has published at least once and for no plan that never has, because a first
draft is minted at revision `0` while a successor presupposes a current revision to succeed
from (§4.3). The sibling of `PLAN_RETIRED_NO_SUCCESSOR` and refused for the same structural
reason, but deliberately not merged into it: that code would assert a retirement that never
happened, and a retired plan still has a current revision, a warm delta and a clone route
forward where this one has none. Not `LIFECYCLE_FORBIDDEN` either, by D-146's own line — the
operator's next action here is specific and different: mint a new plan, and stop retrying this
id — §4.3),
`LIFECYCLE_FORBIDDEN` (422 — a transition the state machine does not permit:
mutating a published row's content, superseding a
grandfathered row. Named 2026-08-02 while implementing §4.3 — the transitions
were normative from the start and the refusal had no code, and a refusal a
consumer cannot discriminate is one it must parse prose to understand; **narrowed the same day
by D-146**, which took out the two refusals a consumer must act on differently and left exactly
the refusals that describe no alternative action),
`TIMESTAMP_PRECISION_EXCEEDED` (422 — an authored instant finer than the millisecond quantum
§2.2 fixes. D-144, 2026-08-02: refused rather than truncated, because truncation moves an
instant a scope-key axis is matched on for equality across a gear boundary),
`ROUNDING_POLICY_UNRESOLVED` (422 — a published row resolves neither a row-level
`rounding_policy_ref` nor a tenant default; the §17.4 no-implicit-rounding rule, registered
into the pipeline by the Foundation itself),
`ROUNDING_POLICY_UNKNOWN` (422 — a row's `rounding_policy_ref`, or the tenant default, names
no **active** value of the tenant's declared rounding vocabulary; **D-334**, 2026-08-16, and
registered into the same base set for its sibling's reason — the column is on every price row.
A tenant that has declared no vocabulary is unconstrained, which is the opposite of
`REGION_UNKNOWN`'s empty set and is stated on both),
`PRIMITIVE_RULES_UNBUILT` (422 — a row carries a value in a field that is **declared and not
yet authorable**: the rules that would judge it are unbuilt, so publish refuses to freeze it
into an immutable ≥ 7-year version. Today that is `tierQualificationWindow` (D-40) and
`includedAllowance` **with `rolloverPolicy = carry`** (D-45) — narrowed 2026-08-15 from
*"`includedAllowance` and `tierQualificationWindow`"*: the six `inst-ac-gate` refusals and the
allowance compile landed together, so `includedAllowance {N, none}` is authorable and this code no
longer reaches it. The `carry` half stays for a reason that is a missing **store** rather than a
missing rule — `inst-ac-carry`'s compiled grant needs `pricing_plan_grant` (D-52), a table this
gear does not have — which is exactly why this code names the **reason** and not the field: no
seventh `ALLOWANCE_*` code was needed for the half nobody can honour yet. Both are refused at
authoring by D-177 clause (1) as malformed
requests with no code — the D-141 class. **D-179**, 2026-08-03: a *stored* row reaching publish
is a different act, its request well-formed and its remedy different (clear the field, not
reshape the payload), and three routes put a value there that no authoring refusal covers —
rows authored before the refusal landed and any write that did not come through a handler.
**The bulk-import arm was the third and is built** (corrected 2026-08-17; it was landed in the
phase program and three documents went on calling it owed): `domain/import.rs` refuses such a
row per-row in Phase 1, reading the same `unjudged_primitives` list publish reads. It names the **reason** rather than the field, because the
roster may grow and the operator's action does not change; and it says *unbuilt* rather than
*invalid* because the value is not wrong — `inst-tt-forbidden`'s objection is to an
accepted-but-ignored value, not to the value itself. Deleted in the same change that lands the
ten Slice-10 refusals and the allowance compile, per D-177 clause (3), and not before),
`CONCURRENT_MUTATION` (409, naming the aggregate — another mutation of this aggregate committed
first, so this transaction's write at a per-aggregate serialization point was refused by the
key; the request was well-formed, its preconditions held, and it may be replayed as sent. **Every aggregate whose write takes an optimistic guard has such a point, and the class is the
contract — not a count (corrected 2026-08-17).** This sentence named exactly three: the
segmented audit chain's `(tenant_id, chain_id, seq)` (§3.7, D-135), the outbox's
per-`(tenant_id, aggregate_id)` sequence (§3.7), and the current-revision partial unique index
(D-90). A client scoping its 409-retry handling to those three would not retry a policy,
taxonomy, membership, migration, bulk, window, approval or price mutation that answers the same
code — a contract **narrower** than the implementation, which is the direction that costs a
write rather than a spurious retry. Re-derive rather than count: `grep -rn 'contention_or_db('
pricing/src/` returns every site that renders this refusal (17 across 9 repository modules at
the time of writing), plus the explicit raises alongside them. **D-159**, 2026-08-03, found while building the
publish commit: the refusal had no code at all and therefore arrived as an internal fault,
indistinguishable from a dead connection — a client told to stop and page someone about a race
whose entire remedy is to try again. Deliberately not `STALE_VERSION`, whose remedy is *re-read
and resubmit* because a caller's precondition genuinely failed, and deliberately not
`IDEMPOTENCY_KEY_IN_FLIGHT`, whose subject is the caller's **own** duplicate; one code over all
three points, the operator's action being identical at each — D-146's line, twice),
`TIER_BAND_PRICE_INCREASE` (**advisory**, never blocking — a tier band whose effective unit
price exceeds its predecessor's; the rule, its sorted-geometry clause and its
free-opening-band carve-out are [`03-price-structure.md`](./03-price-structure.md)
`inst-tb-order`'s and are unchanged. **D-160**, 2026-08-03: the rule and the channel existed and
the code did not, so the implementation had to invent a token to put in a report — this
declares the one it invented),
`PLAN_SIZE_SOFT_CAP_EXCEEDED` (**advisory**, never blocking, naming the cap, its limit and the
count — a plan above the per-tenant tier-bands-per-row or price-rows-per-plan soft cap held in
`pricing_policy_object` (§3.7, D-152; the ratified 100/500 defaults for a tenant with no
entry). **D-160**: [`../PRD.md`](../PRD.md) §7.1 `nfr-size-limits` has said since ratification
that the system SHOULD enforce these "emitting a publish warning above the cap", and no
document named an advisory code, so the requirement had nothing to be reported through and was
built as nothing. One code for both caps — the remedy has one shape, and the report says which
cap. The **interval** caps are a different kind and keep `INVALID_CUSTOM_INTERVAL`: they are
hard), the shared money checks (§2.2) —
`PRECISION_EXCEEDED` (422 — precision above the currency's ISO 4217 minor unit),
`AMOUNT_NEGATIVE` (422 — amounts are ≥ 0), `CURRENCY_INVALID` (422 — not a valid ISO 4217
code), the aggregate validation
report envelope (422 — enumerating blocking `violations[]` plus advisory `warnings[]`), and
publish-accepted/pending (202).

**Status rendering — the 422s above are architectural, not wire (normative).** The `422`
annotations in this set say *unprocessable content*: the request was understood and the
catalog refuses to publish it. The platform's `CanonicalError` model has **no 422 category**
at all (`InvalidArgument`, `FailedPrecondition` and `OutOfRange` all render **400**), so every
architectural 422 in this design set — here and in every slice — reaches the wire as a **400
carrying its wire code**, and **the code string is the discriminator a consumer matches on,
not the status**. This is the sibling ledger's rule verbatim
(`../../ledger/docs/design/02-audit-immutability-observability.md`), and the reason it is
stated once here rather than per occurrence. Two consequences bind the implementation: a
rejection is classified by what it *is*, so a retriable conflict on mutable state stays a
**409** (`ABORTED`) rather than collapsing into the 400 bucket — the **six** conflicts above are
exactly that class (three until 2026-08-02, when `IDEMPOTENCY_KEY_IN_FLIGHT` (D-143) and
`OPEN_DRAFT_REVISION_EXISTS` (D-146) joined them, and `CONCURRENT_MUTATION` (D-159) on
2026-08-03; each was classified by this rule, not by the
section it was found in) — and an endpoint MUST NOT declare a 422 response in its `OpenAPI`
registration, because no path can produce one.

**The advisory channel is code-carrying too (normative, D-160, 2026-08-03, found while building
the publish rule set).** The validation report envelope above has two arrays — blocking
`violations[]` and advisory `warnings[]` — and they differ in whether publish proceeds, not in
whether a finding is nameable: **every advisory carries a declared code**, from this same
catalogue, and never re-uses a violation's. The reason is the status-rendering rule one
paragraph up, applied to the half it did not mention: if the code string is the discriminator a
consumer matches on, then an advisory without one is a sentence each consumer must parse, on
the channel whose whole point is that a client can act on it without doing so. This set states
exactly two advisories and had named a code for neither, which cost more than tidiness in one
of the two cases — [`../PRD.md`](../PRD.md) §7.1's ratified soft caps had **nothing to be
reported through**, so a `SHOULD` allocated to the publish engine in §1.2 was not merely
unbuilt but unbuildable. Both are declared above.

**Collection pagination (normative, D-125, 2026-08-01):** every collection, history and audit
read surface of this gear **paginates**: `limit` (server default 100, hard cap 1,000 — the
unit the export SLO is expressed in) plus an **opaque `cursor`**, with `next_cursor` returned
on every page until the result is exhausted. Ordering is **stable and append-consistent** —
commit/append order on history and audit reads, so a cursor walk concurrent with writes never
skips or duplicates a row at or before the cursor; a deterministic key order on catalog lists.
Offset/`$skip` pagination is not offered (unstable over append-only stores at the ≥ 7-year
retention). Slice surfaces (`/bss-pricing/v1/plans*`, `…/prices`, `/bss-pricing/v1/price-overlays`,
`/bss-pricing/v1/approvals`, `/bss-pricing/v1/history`, `/bss-pricing/v1/audit`, batch reports' row lists)
inherit this contract rather than restating it; exports stream the same order in bounded
chunks, and the p95 ≤ 5s / 100 records SLO applies **per page/chunk** — before D-125 that SLO
was expressed per page while no page contract existed anywhere in the set. Two things the
cursor is not, stated here rather than discovered (2026-08-03, found while building the
authoring list surface — cleanup, no decision id). A cursor the gear cannot decode is a
**malformed request** under the validation envelope above, answered 400 with no code of its
own, exactly as an absent precondition is (D-141); nothing else is available, since an
undecodable token names no position to resume from. And the walk is **not a snapshot**: the
guarantee is that no row at or before the cursor is skipped or duplicated, which a row
*deleted* during the walk does not break in either direction — one deleted behind the cursor
was already returned, one deleted ahead is never returned — so a client that needs a
consistent view of a mutating draft plane is asking for a transaction spanning HTTP requests,
which this gear does not offer.

**Preconditions on the wire (normative, D-171, 2026-08-03):** every §5 API table in this set
carries an **Idempotency** column, and until now no document named the request header any of
its cells travels in. They are named once here, and slice tables inherit the mapping rather
than restating it, as they inherit the pagination contract and the route shape. An **`ETag`**
cell means a **required `If-Match`** request header carrying the entity tag the addressed
resource's own read returned (RFC 9110's precondition field; the plan plane's tag shape is
D-170, the price plane's is D-141). A **`client idempotency key`** cell — and its variant
spellings `idempotency key` and `idempotency key / ETag` — means a **required
`Idempotency-Key`** request header whose value is `pricing_idempotency_dedup`'s `client_key`
(§3.7). A cell naming a **natural** idempotency (`per revision`, `per plan revision`, `per
decision`) requires **no header**: the call is idempotent on a key the request already
carries, and there is nothing for a caller to mint. `—` means the surface is not a mutation.
Both headers are **required, not advisory** — an absent one is a malformed request under the
validation envelope, no new code, which is the rule D-141 already stated for the absent
precondition.

**A precondition is evaluated inside the transaction that performs the mutation it guards
(normative, D-176, 2026-08-03, found while auditing what the authoring surface's tag check
actually holds).** This set calls the `If-Match` mechanism a **compare-and-swap** in three places
and never said *where* the comparison happens, which is the whole of its strength: a comparison
made before the mutation's transaction opens is a **hint**, not a precondition, because the state
it read can move between the two. For a mutation whose write is an **UPDATE** — every price-row
verb, every facet edit of an existing draft revision — that is the compare-and-swap the set
already names, the tag being matched in the statement that writes. For a mutation whose write is
an **INSERT**, the tag names a *different* row than the one being written, and the rule is the
same: the guarding row's identity and version are re-read **inside** the writing transaction and
compared there. This gear has exactly one such mutation today, the `PATCH` arm that opens a
successor revision on a published plan ([`02-plan-definition.md`](./02-plan-definition.md) §5,
D-170): the caller's tag names the **current** revision, the successor is copied from it, and if
that comparison sits outside the transaction that inserts, a concurrent publish landing between
them leaves the successor copied from a revision the caller never read while the call answers as
though its precondition had held — D-145's lost update, arriving through the copy source instead
of the row version. Two consequences are normative rather than incidental. **(1)** The successor
open and the facet write the same call performs are **one** transaction: two transactions cannot
carry one precondition, and the intermediate state is operator-visible rather than harmless — an
open draft at `max+1` occupying the plan's single editable slot (§3.7), which every subsequent
`PATCH` from **any** operator then takes instead of the successor arm, from a call that answered
with an error. **(2)** The audit record of a mutation being inside its own transaction (D-135) is
a *separate* obligation from this one and does not discharge it: the record says a revision was
minted, not that the caller's precondition was true when it was. Rejected: **(a)** leaving the
comparison outside and documenting the window, since a documented lost update is still a lost
update and the set's own word for the mechanism is a swap; **(b)** fusing the two transactions
without giving the insert its expected version, which closes the partial outcome and leaves the
window exactly where it is — half a fix that reads as a whole one; **(c)** `SERIALIZABLE`, on the
same merits §4.2 declines it (D-155 clause 2) — it buys nothing the tags buy and would hold
predicate locks across paths this gear deliberately keeps short.

**Route shape (normative, D-140, 2026-08-02):** every REST surface of this gear is served
under the gear's service prefix — `/bss-pricing/v1/{resource}`, where `bss-pricing` is the
registered gear name — and an action on a resource is a **sub-resource segment**
(`…/{id}/publish`, `…/{id}/start`), never a colon-suffixed custom method. Slice surfaces
inherit this shape rather than restating it, as they inherit the pagination contract above.
The convention is the platform's (`/{service-name}/v{N}/{resource}`, the sibling ledger's
`/bss-ledger/v1/…`) and is enforced mechanically at build time, so it binds the documented
paths as well as the implementation.

### 3.4 Internal Dependencies

- **`toolkit-db`** — transactional persistence for the append-only history, the owned window store (Slice 7, D-03), the projected read model, the outbox, and the audit store.
- **Coordination lease library** — singleton coordination for read-model warm re-drive and the window activation/expiration job (Slice 7).

### 3.5 External Dependencies

- **Catalog registry (Product & SKU)** — published `skuId`, `bundle` SKU type, `meteringUnit` declaration, `PlanTier` taxonomy; the **sole** `CatalogVersion` incrementer. Bidirectional `CatalogVersion`-increment contract (`cpt-cf-bss-pricing-contract-registry-catalogversion`).
- ~~Effective-dating PriceWindows use case~~ — **consolidated into this gear** (Slice 7 owns the window store, state machine, activation job, and `PriceWindow*` emission — D-03); `cpt-cf-bss-pricing-contract-pricewindow` is thereby internal, not an external boundary.
- **Tariffs / Subscriptions / Rating / Billing** — consume the read model / events; their payloads are refined in the consumer-contracts and capability slices.

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-seq-author-publish-fnd`

**Author → validate → approve → publish → CatalogVersion.** A slice authors/updates draft rows
(ETag checked, idempotency-deduped); submit runs the ValidationPipeline as a **pre-check** and
routes a material change through the Slice 5 approval gate; on approval (or immediately for a
below-threshold change) the publish commit **re-runs the aggregate rule set inside the commit
transaction** (approval approves content; the commit re-validates state — a failure at commit
voids the approval and returns the subject to draft with the report); on success — and **in
this order** (normative, **D-156**, 2026-08-03, found while building the commit; this sentence
previously enqueued before requesting, which §4.2 step 4 makes unbuildable, the payload having
no handle to carry) — the SnapshotStamper **requests a `CatalogVersion`**, the row set
transitions to `published` (append-only), and the EventOutbox enqueues `PlanPublished` with the
**pending** version ref. The request is inside the commit transaction, after the re-validation
and before the writes: earlier strands a pending ref that nothing commits whenever the
re-validation refuses (tripping the overdue alarm below for a publish that never happened), and
later leaves the payload empty. It buys that with a **network round-trip inside an open
transaction**, bounded by a `request_id` derived from `(tenant_id, plan_id, revision)` — a
retried commit re-requests the same handle instead of orphaning one — and by asking the one
permanently-unpublishable case (a retired predecessor) *before* the request. The registry batches approved publishes and emits
`CatalogVersionPublished`. **A batch commits atomically into one version, and that is a
property of the registry contract rather than an assumption of this gear (normative, D-163,
2026-08-03, found while building the read side):** one batch, one version, one event, so a
version's subject set is **closed** the moment any one of its refs commits and the only open
question about that version is warmth. §4.4's pin-eligibility is not decidable without it —
"every subject row that version projects" names a set nobody can enumerate while the set is
still growing. The ReadModelProjector warms the projection and marks completion, or
the publish is marked degraded (`PlanPublishDegraded`). `pricingSnapshotRef` pins the committed
version. No intermediate state exposes a rateable-but-incomplete plan. A
`pricing_catalog_version_ref` still `pending` **and not yet observed committed** past the max
batching-delay SLO raises a
Critical alarm (`pricing.catalogversion.commit_overdue`) and surfaces on the publish status
API; a `CatalogVersionPublished` batch that omits an expected pending ref is treated the same
— remediation is a registry re-request, never a silent re-emit. **The qualification is D-166's
and it is what keeps the three signals disjoint:** a ref whose version this gear *has* observed
committed is no longer waiting on the registry, and a warm that then fails to land is
`PlanPublishDegraded`'s condition (§4.4), not this alarm's. Neither state goes unreported —
`pricing.readmodel.pin_eligibility_overdue` (§4.4) covers a frontier held by either.

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-seq-readmodel-resolution-fnd`

**Consumer read-model resolution.** A consumer pins one **pin-eligible** `CatalogVersion` (§4.4,
D-101 + D-114: committed, every subject row of that version warm-complete, *and* every earlier
version itself pin-eligible) and resolves the
complete frozen read model via `pricingSnapshotRef` — no draft read, no default substitution,
monotonic per version; the pin never lags the newest pin-eligible version by > 5s.

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-schema-fnd`

Foundation-owned tables (tenant-scoped, SecureORM). Money columns are integer minor units at
the currency's ISO 4217 scale. **Table-naming discipline (normative):** every physical table
of this gear carries the gear-name prefix **`pricing_`** — matching the sibling ledger's
`ledger_*` convention (`ledger_journal_entry`, `ledger_idempotency_dedup`, …). Domain entity
names (`Plan`, `Price`, `ReadModel`) stay unprefixed; the prefix applies to physical tables
only, including slice-owned tables in every other slice design.

- **`pricing_plan`** — keyed **`(plan_id, revision)`** (composite PK — the draft-revision-row model, D-56, 2026-07-28 review fix, confirmed 2026-07-31), `tenant_id`, `sku_id`, `plan_tier`, `billing_cycle`, `lifecycle_state` (`draft`|`published`|`superseded`|`retired`|`abandoned` — `superseded` added by D-90; the enumeration had omitted it in the same paragraph that describes the flip, 2026-07-31 review fix; `abandoned` added by **D-145**, 2026-08-02, the terminal state a discarded draft revision takes instead of being deleted), `available_from`/`available_to`, `created_by` (pseudonymous principal id of the authoring actor — 2026-07-28 review fix: the Slice-12 history surface reads it under `plan × read`, so actor identity never requires the Auditor-only `pricing_audit_log`), ETag/row-version. Published revision rows are immutable **in content** — their only sanctioned in-place mutations are the state-machine `lifecycle_state` flips (`published → superseded` when the next revision's publish commits — **D-90, 2026-07-31 review fix: the plan-revision analogue of the price rows' flip-at-commit**, so a partial `UNIQUE (plan_id) WHERE lifecycle_state IN ('published', 'retired')` (**widened by D-128**, 2026-08-01 review fix — the predicate previously read `= 'published'`, which held no row at all once retirement flipped the only current revision) holds at most one **current** revision and "the current revision" is unique by construction for the projector, the sellability lifecycle predicate, and every referential check — and `published → retired` on the current revision at retire, a **publish unit** in its own right (D-128, §4.2); D-56's own framing: in-place mutation survives only as state-machine flips — 2026-07-30 review fix); only the plan's single open `draft` revision row mutates content, through DraftStateMachine transitions (partial `UNIQUE (plan_id) WHERE lifecycle_state = 'draft'`; `lifecycle_state` per §3.2), fully audited — the normative revision model is §4.3. **`revision` is an identity, not a counter (D-145, 2026-08-02, found while building the draft-authoring plane):** it is minted `max(revision) + 1` over the plan's own rows and **never re-minted**, so a discarded open draft is flipped to the terminal `abandoned` state rather than deleted (its child copies dropped, the flip audited exactly as the deletion was) and the number it consumed stays consumed — deleting the row freed the number, and a re-minted `(plan_id, revision)` is a durable name pointing at two distinct rows, under which a stale ETag passes its precondition against the wrong one (§4.3 carries the argument and the rejected alternatives). `abandoned` is terminal and sits **outside both** partial `UNIQUE` predicates on this table — outside `WHERE lifecycle_state = 'draft'`, so a replacement draft opens immediately **on any plan that has published at least once** (a plan whose *only* revision is `abandoned` is the exception, and the index is not what stops it: §4.3), and outside `WHERE lifecycle_state IN ('published', 'retired')`, so "the current revision" is untouched — which is what lets the state be added without disturbing D-90's or D-128's uniqueness guarantees; **child shape tables version with the revision, copy-on-new-revision (D-83 — [`02-plan-definition.md`](./02-plan-definition.md) §6)**. **Physical enforcement extends beyond `pricing_price` (2026-07-31c review fix, L-2):** published plan-revision rows and every revision-scoped child/composition table (S2 phases/add-on rules/descriptors, S8 bundle tables, S9 overlay revisions/lines, S10 grants/composites) carry the same `REVOKE` + column-whitelist trigger discipline — permitted UPDATEs are exactly the sanctioned `lifecycle_state` flips on the revision-carrying rows; child rows are physically immutable once their revision publishes (draft-revision rows and their copies stay freely mutable/deletable). The projector's re-drive reads truth rows (§4.4), so without the physical guard an unsanctioned UPDATE would silently change a frozen version at re-warm — the same argument that put the trigger on `pricing_price`. **A plan route's entity tag names the revision as well as the version (D-170, 2026-08-03, found while building the authoring REST surface).** The row-version column above is per **revision row**, and a plan route addresses a *plan*, resolving to its open draft revision if it holds one and to its current revision otherwise (§5 of [`02-plan-definition.md`](./02-plan-definition.md)) — so a tag carrying only a row version cannot say which of the two it was read from, and the counters are unrelated: a freshly opened successor stands at the initial version, exactly where a first draft stands, so a tag minted against revision *N* satisfies the compare-and-swap on *N+1* with **no race window at all**. That is the lost update §4.3's identity rule (D-145) removes from storage, reopened in transport by addressing a plan without naming a revision. The tag therefore carries **both** — the gear renders it `"<revision>-<version>"`, two decimals joined by a hyphen inside the quoted entity-tag string — and a mismatch in **either** component is `STALE_VERSION` (§3.3), the same refusal in substance, the caller working from a read it did not refresh. The tag is **opaque to the caller**, copied back verbatim into `If-Match` (D-171) and never constructed or parsed; a wildcard, a weak validator, a list or any other token is a malformed request. `pricing_price`'s tag is deliberately **not** qualified this way: a price route addresses one row by `priceId`, so the tag and the row are one to one and D-141's bare row version has nothing to disambiguate.
- **`pricing_price`** — `price_id` (PK), `tenant_id`, the **canonical scope-key columns** (`plan_id`, `currency`, `region`, `price_overlay`, `phase`, `price_eligibility`, `charge_kind`, `cohort` — `none` unless `existing_grandfathered`, ADR-0002), `amount_minor`, `model_kind`, `tax_inclusive`, `billing_timing` (recurring), evaluation-policy columns (usage), `rounding_policy_ref` (nullable named rounding-policy id — PRD §17.4: publish resolves the row-level reference, else the tenant default from `pricing_policy_object`, else fails `ROUNDING_POLICY_UNRESOLVED`; the **resolved** policy id freezes into the read model / snapshot, definition + application stay downstream in Tariffs/Billing), `tax_category_ref` (nullable — Slice 4's column and the sole per-row source of truth, D-110; **the resolved *effective* category freezes into the read model / snapshot exactly as the rounding policy does** — **D-154**, 2026-08-03: the descriptor contract owes Billing a category it can post without re-querying, and the fallback half of that value lives in the mutable per-`(tenant, region)` region taxonomy), `grandfather_until`, `supersedes_price_id`, `lifecycle_state`, `created_by` (pseudonymous authoring principal — the history-export actor field, Slice 12; **D-12**, whose 2026-07-28 amendment makes this the source of actor identity so the history surface never requires the Auditor-only `pricing_audit_log`), **ETag/row-version** — the row's **own** optimistic-concurrency token (**D-141**, 2026-08-02, found while building the draft-authoring plane). It is never derived from the plan's: a per-row bulk conflict means nothing if every row of a plan shares one version, and three surfaces already required the token to be per price row while this section declared it on `pricing_plan` alone — S3 §5's `PATCH …/prices/{priceId}`, the bulk import's "existing **draft** rows under **their** ETags" whose conflict fails "**only that row**" ([`12-operator-efficiency.md`](./12-operator-efficiency.md) `inst-bk-phase2`, D-118), and `inst-co-single-pending`'s "**ETag protects rows**, this rule protects change units" ([`07-pricewindow-linkage.md`](./07-pricewindow-linkage.md)) — so a reader building the table from here gave `pricing_price` no version column at all and all three rules became unimplementable. **Every** mutating verb on a `draft` price row presents it: `PATCH` (unchanged) and `DELETE` (new — its S3 idempotency cell was empty, so a draft row could be destroyed under an unknown version, reopening on the one verb that leaves nothing behind to reconcile the lost update `fr-concurrent-edit` closes for `PATCH`). A mismatch is `STALE_VERSION` (§3.3); an absent precondition is a malformed request under the existing validation envelope — **no new code**. The scope is deliberately `pricing_price` draft rows: `DELETE …/price-windows/{windowId}` carries an empty cell too and is **not** moved, because window cancellation is an always-material publish unit (D-62, D-99) whose concurrency is governed by `inst-co-single-pending`. **The scope-key columns carry `meter` and `dimension_key` too, on usage rows (D-196, decided by the product owner 2026-08-06; §4.1 carries the axis statement).** Before it, the eight axes rendered one key for every usage line of a plan in one market and the second line was refused `DUPLICATE_SCOPE_KEY` at save — measured, not inferred — so D-103's confirmed worked example was unstorable while three sites blessed it. **The physical mechanism is an expression index over a sentinel, and the naive spelling would have failed silently in the opposite direction.** `meter` is nullable (`dimension_key` is `NOT NULL DEFAULT ''`), and NULLs are **distinct** inside a `UNIQUE` index — so simply listing the column in the two indexes below would not merely leave usage rows unconstrained, it would **destroy the uniqueness they have today on every non-usage key**, all of which carry `meter IS NULL`. Measured on SQLite 2026-08-06: a naive `UNIQUE (a, meter, dim)` admitted two rows on one key, and the same index over `COALESCE(meter, '')` refused the second while still admitting `cloudlets` and `egress_gb` as two keys. **Both scope-key indexes therefore key over `COALESCE(meter, '')` and `dimension_key`**, which keeps one index per plane instead of splitting each by charge kind, and leaves untouched the one place a NULL `meter` carries meaning — `uq_pricing_price_meter_line_current`'s `WHERE meter IS NOT NULL`, which distinguishes "not a metered line" from "a line with no meter". The empty string is available as a sentinel only because the `meter` value itself may not be blank. **The meter-line index is not subsumed by the widened key and stays**: it excludes `charge_kind` on purpose, so it forbids one `(meter, dimensionKey)` line being priced twice across *different* charge kinds, which the scope key still permits. **Owed to the implementation as of 2026-08-06** — D-196 carries the four clauses and the Postgres proof owed for this paragraph's claim on that engine. **Two partial `UNIQUE` indexes over the scope key, one per plane, with disjoint predicates.** (1) The **published**-plane index (`WHERE lifecycle_state = 'published'` — sufficient on its own under the flip-at-commit rule (§4.3: the predecessor reads `superseded` the instant its successor commits), and the only expressible form anyway — a partial-index predicate sees the row's own columns, and the supersession link lives on the **successor** row; 2026-07-30 review fix) enforces at most one current row per key — **temporal `PriceWindow` non-overlap and coverage are enforced by the publish-time validation pipeline (Slice 7, gear-owned per D-03), not by this index**, so a **superseded** predecessor (its window still active until the changeover) and its published successor with a scheduled window legally coexist. (2) The **draft**-plane index (`WHERE lifecycle_state = 'draft'`) guards the authoring plane (**D-148**, 2026-08-02, found while building it). Nothing covered that plane, while D-21 puts scope-key duplication in the **save-time** row-local set — a check decided by a read — so two concurrent creators on one key both read "absent", both insert, and the duplicate goes undiscovered until one of them publishes, at which point the operator is told, correctly and uselessly, that a row they authored days ago collides with one they cannot see. That is the opposite of what D-21's save-time placement is for. Under the index, D-21's read stays the fast, explanatory path and the index becomes the guarantee — the read-then-index arrangement the published plane already has — and a violation renders as the existing `DUPLICATE_SCOPE_KEY` (§3.3): **no new code**. The two predicates are disjoint by construction, so a draft successor legitimately coexists with the published row it will supersede, and the only-expressible-form argument above is untouched — it was an argument about which rows the *published* index can see, never an argument against a second predicate. **Nor can a staged composition put a *second* draft on the key** (verified 2026-08-02 against the composition paths, recorded in D-148's body): the supersession unit composes only on a key whose predecessor window it can shorten ([`07-pricewindow-linkage.md`](./07-pricewindow-linkage.md) `inst-su-compose`, which fails compose on a dormant key), so the key already carries a `published` row — which is exactly what makes a hand-authored draft on it impossible, refused at save as a duplicate active scope key ([`03-price-structure.md`](./03-price-structure.md) `inst-pr-return`, D-21) and refused per-row on the bulk plane (`IMPORT_TARGETS_PUBLISHED`, D-118); a second *unit* on a held key is refused by `inst-co-single-pending`, and both bulk mechanisms fail per-row against a pending one (`inst-bk-phase1`, `inst-mp-pending`, D-35). **This argument rests on two doors, and D-195 (2026-08-05) says which each one refuses.** It reads above as though the authoring door's refusal alone bounded the key, but the composition has to insert its successor draft *somewhere*, and the door it inserts through cannot be the one that refuses a published occupant. So occupancy is a property of the **door**, not of the table: the authoring door refuses a `draft` **or** a `published` occupant (`inst-pr-return`, D-21 — unchanged, and the publish-time row flip's single-UPDATE safety argument cites exactly this), while the supersession door's precondition is the mirror image — it **requires** a `published` occupant and **refuses** a `draft` one (`07-pricewindow-linkage.md` `inst-su-compose`). Each refuses the occupant the other requires, so one draft per key stays the most the pair admits, which is what this paragraph claims; the ordering that keeps the *published* plane to one row through the commit is `inst-su-commit`'s. The cutover stages nothing here at all — it is "not a table" but an approval-unit payload, and its successor is **inserted** on the published plane inside the commit (D-100), its grandfathered copy landing on a **new generation key** (`inst-co-copy`). So this index refuses exactly what D-21's save-time read cannot catch: two concurrent creators on a key no published row occupies — **and, since D-195, two concurrent composers on a key a published row does occupy**, the supersession door's own read being a read like any other. The "and nothing else" this sentence used to carry was true only while the authoring door was the only door (corrected 2026-08-05). Rejected: widening the published index to `IN ('draft', 'published')`, which would make a draft successor collide with the very row it supersedes. Consequence for bulk import (D-118 — draft plane, per-row commits): a batch carrying two rows on one key now fails the second **on the index** rather than admitting both, and `inst-bk-phase1`'s per-row report names that case. Append-only via `REVOKE UPDATE, DELETE` + `BEFORE UPDATE/DELETE` trigger with a **column whitelist**: the trigger rejects any UPDATE of a published row except (a) `lifecycle_state` transitions permitted by the state machine (`published → superseded` on supersession/cutover) and (b) monotonic tightening of `grandfather_until` (setting it when null, or moving it earlier); all price/scope/model columns **and the row-version column** are immutable and DELETE is always rejected — controlled transitions run through the engine's transition path, never ad-hoc SQL. **The draft plane is guarded for transitions too (D-153, 2026-08-03).** A column whitelist is scoped to *published* rows by construction, so it says nothing about where a **draft** row may go, and the price row's state machine ([`03-price-structure.md`](./03-price-structure.md) §4) has exactly one edge out of `draft` — to `published`. A draft row moved straight to `superseded` would satisfy every constraint on this table and land **outside both** partial `UNIQUE` predicates above: its key would read free on the published plane *and* on the draft plane, so the guarantee D-148 had just bought — the second concurrent creator is refused — is undone by one UPDATE, and `inst-ps-nodelete` then makes the ghost undeletable, on a key no supersession chain reaches because the row was never current. The trigger therefore constrains the draft row's `lifecycle_state` as well: `draft → draft | published` and nothing else, exactly as `pricing_plan`'s does for its own state set (`draft → draft | published | abandoned`, D-145). **No new code** — no API offers the transition and no caller can provoke it; this is the physical floor under a state machine the engine already honours, the same posture as the D-148 index. **The row version freezes with the published row's content (D-141, 2026-08-02).** It joins the frozen whitelist beside the price/scope/model columns, and neither sanctioned in-place mutation moves it: not the `lifecycle_state` flips, not the monotonic `grandfather_until` tightening. An entity tag that moved under a representation the engine forbids changing would tell a client its cached copy is stale when it is not, and the tag exists for the **draft** plane, where content really does change and D-141's precondition rule binds. **The reason this paragraph originally gave for scoping that rule to draft rows — that the published plane has no caller-driven mutation for a precondition to guard — stopped being true on 2026-08-16 (D-329), and the freeze it justified survives anyway.** The grandfathering horizon's authoring door is exactly such a mutation, and S7 §5 gives it an `If-Match` cell. What guards it is not the tag, which is constant across every tightening and so cannot refuse a stale horizon, but a compare-and-swap on `grandfather_until` itself: **a monotone field is its own version**, and unlike a row counter it cannot be tripped by a write to an unrelated column. The freeze therefore stands on the narrower ground it always had — an entity tag that moved under a representation the engine forbids changing would tell a client its cached copy is stale when it is not — and D-190's counter column becomes necessary only if a caller-driven published-plane mutation lands that is **not** monotone.
- **Price history** — history is the set of superseded rows retained **in `pricing_price` itself**, keyed by `supersedes_price_id`; no rows are ever moved or deleted (no separate history table).
- **`pricing_read_model`** — the projected frozen view keyed by **`(tenant_id, catalog_version, subject_kind, subject_ref)`** with a per-row `warm_completed` marker; monotonic per `catalog_version`. **Storage is a per-subject delta (D-86, 2026-07-30; subject-typed by D-91, 2026-07-31)**: `subject_kind ∈ {plan, price_overlay, overlay_index, group_membership}` (extensible), and a version's rows are exactly the subjects of the publish units that produced it — never a full tenant copy (≤ 5s interactive coalescing would explode one). A plan publish projects its plan-subject row (`subject_ref = plan_id`, exactly the D-86 semantics); a **`PriceOverlay` publish unit projects one overlay-subject row** (the overlay document: lines, amounts, dating, disclosure, lifecycle) and **never re-projects targeted plans** — Tariffs joins overlays to base rows at evaluation per the §9.2 contract, so a `global`-scope overlay commit writes one row, not a tenant's worth; a **membership publish unit projects one membership-subject row** per payer record (D-06's units thereby have a defined read-model representation). An overlay publish unit additionally re-projects an **`overlay_index`** subject — the live overlay id set with each overlay's interval and precedence (**D-112**, 2026-07-31 review fix): per-subject resolution answers "overlay X at pin V" but evaluation needs the *set*, and without an index the only path was a `DISTINCT subject_ref` scan over ≥ 7 years of overlay deltas on the order-time p95 < 100ms path ([`09-price-overlays.md`](./09-price-overlays.md) §7). **The index is sharded and horizon-bounded (D-133, 2026-08-01 review fix):** `subject_ref = (scope_class, scope_value)` (a `global` sentinel value for the classless one), and a shard carries only overlays whose own interval intersects `[projection_time − H, ∞)` on the D-121 horizon. D-112's accounting — "two delta rows per commit, still O(publish units)" — counted **rows and not bytes**: as a single tenant-wide document the index was O(live overlay count) per row, rewritten whole on every commit and retained on the ≥ 7-year truth horizon, i.e. O(commits × overlays) of storage and O(overlays) of write amplification per commit, on the object the order-time read path touches. Sharded, a commit rewrites exactly one shard (two, when a revision moves the overlay's scope value), and an evaluation reads the ≤ 6 shards its payer context can match as point lookups. Resolving `(pin V, subject S)` reads S's row with the greatest `catalog_version ≤ V` whose `warm_completed` is set (one indexed read on `(tenant_id, subject_kind, subject_ref, catalog_version DESC)`, inside the p95 < 100ms budget). **Retention**: delta rows are retained on the same horizon as the append-only truth history (≥ 7y, jurisdiction-configurable, audit-aligned) — growth is O(publish units), the truth tables' own order; compacting superseded deltas beyond the horizon is an ops knob, never a semantics change. **Per-delta size** is bounded by the D-121 projected-set rule (§4.4): a plan delta carries the rows/windows intersecting the `H` horizon, never the plan's whole accumulated history.
- **`pricing_catalog_version_ref`** — `pending` vs `committed` version linkage per publish, **plus the subject the publish unit projects: `subject_kind` + `subject_ref`** (**D-157**, 2026-08-03, found while building the publish commit). The kinds are `pricing_read_model`'s own universe above — `plan | price_overlay | overlay_index | group_membership`, extensible — deliberately not a second spelling of the same aggregates, so the projector's input and its output are keyed alike. Without them the projected-row-set rule one bullet up is unimplementable: a version's rows must be "exactly the subjects of the publish units that produced it" (D-86/D-91, §4.4), and the ReadModelProjector arrives at `CatalogVersionPublished` holding **committed refs** — with no subject on the ref row there is no path at all from a handle back to what it published. Rejected as the carrier: the **outbox** row, whose `aggregate_id` is the plan and whose payload holds the handle, so the join exists — but that makes a delivery queue the projector's durable index, and the first compaction of delivered history removes the projector's input; and re-deriving the subject set from the truth tables at `CatalogVersionPublished`, which re-reads a world that has moved (the D-155 defect, one section over). **Owed by Slice 9:** one column pair holds **one** subject and an overlay publish unit projects **two** — the overlay document and the D-112/D-133 `overlay_index` shard, three rows when a revision moves the overlay's scope value — so that build widens this to a subject **set**. **The instants are named here because the design set's own alarms measure them** (2026-08-03 cleanup, no D-number — §3.6 conditions `commit_overdue` on "a ref still `pending` past the max batching-delay SLO" and named no column to measure the age from): `requested_at`, stamped by the publish commit that asked for the handle, and `committed_at`, stamped by the finalize. **`commit_observed_at` joins them (D-166):** the instant this gear first saw the registry answer for this ref, written when the registry answers and **independently of whether the projection then lands**, which is what makes the post-commit warm SLO — and therefore the degraded condition `fr-publish-fanout-atomicity` states — measurable at all. **The version column is deliberately not unique (D-163):** one version carries the whole batch, so the mapping version → ref is one-to-many and the index over `(tenant_id, catalog_version)` is the projector's every-ref-of-a-version read and the frontier walk's next-version read, never a bijection claim. **The ref also carries the content coordinates its own publish judged (D-165, 2026-08-03):** `subject_revision` and `subject_lifecycle_state`, both nullable because only a revisioned subject kind has them, `subject_lifecycle_state` constrained to the two tokens D-128 sanctions for a projected subject (`published`, `retired`) so `superseded` is not expressible in a ref at all; a `plan` subject arriving without them is **refused**, never defaulted. **`draft` is excluded by the same clause and for a second reason (D-314, 2026-08-15):** `superseded` is a state a *frozen* revision drifts into after its pin and is kept out so a consumer does not read the version as unsellable, while `draft` is a state whose row is still **mutable** — the projector reads a pinned revision's content live off the truth row up to the max batching-delay SLO after the commit, and that licence rests on the row and its revision-scoped children being physically immutable (§4.4), which is true of a published revision and false of a draft. So a draft-pinned ref would freeze un-judged, still-moving content into an INSERT-only delta on the seven-year horizon. This clause is what makes the window surface's refusal on a never-published plan a rule rather than an artifact of how that surface looks the revision up ([`07-pricewindow-linkage.md`](./07-pricewindow-linkage.md) §5), and a relaxation attempted at the domain layer alone aborts here, at the write, which was measured.
- **`pricing_pin_frontier`** — PK `tenant_id`; `catalog_version` + `advanced_at`. The materialized **pin-eligibility frontier** (D-136, §4.4): advanced only forward, and only by the ReadModelProjector inside the transaction that completes the frontier's next version in order — **and then walked forward through every immediately-following version that is already complete (D-164, 2026-08-03)**, without which a version completed out of order stands complete behind the frontier forever, never seeing another completion to advance on. The PK being `tenant_id` alone is also what settles the reading of D-114's prefix: the walk is over **this tenant's** committed versions, not over the cross-tenant `CatalogVersion` sequence (D-164, §4.4). It is what `GET /bss-pricing/v1/catalog-version/frontier` serves, what the ≤ 5s pin-lag rule is measured against, and what `pricing.readmodel.pin_eligibility_overdue` reads — the D-101/D-114 predicate is otherwise a recursive scan of the delta store on the p95 < 100 ms path, with no owner and no surface. **`advanced_at` is the alarm's referent but not its whole predicate (D-166):** a tenant that has simply not published is stale by construction and is not a fault.
- **`pricing_policy_object`** — the approval-threshold and tax-display policies (fail-safe defaults), the tenant **default rounding policy** (a named rounding-policy id; optional — a tenant without one simply requires every published row to carry its own `rounding_policy_ref`, per the §17.4 fail-closed rule — **written through `GET/PUT /bss-pricing/v1/config/rounding-policy`, D-320, 2026-08-15**: until then the column had no writer at all, so the fail-closed arm was the only reachable one and the per-row reference read as mandatory rather than as the override it is), the **enforced-migration notice period** (days; default floor 60 — D-49, validated by Slice 11 at scheduling), and **every per-tenant configurable this gear promises** (**D-152**, 2026-08-03, found while building the Slice-2 validators): the descriptor required-set extension (additional required descriptor keys, matched against `pricing_plan_descriptor_set.additional_fields` — S2 `inst-ds-sufficient`, P5) and the §14 soft caps and interval caps (tier bands per row, price rows per plan, `customEveryNDays`/`customEveryNMonths`). Those numbers and that extension were each described as tenant-configurable in a ratified NFR or a pinned assumption while **no document named where they are declared**, so a promise of per-tenant configuration had no carrier and the gear's own configuration section is per **deployment**; this bullet is the carrier, and a tenant with no entry takes the ratified launch default. Nothing here is on a resolution path: these are authoring-time policy reads, like the two that were already here. **The carrier is provisional for those two additions** (the D-152 veto confirmation, 2026-08-03): the product owner confirmed this table as the home of the descriptor required-set extension and the four §14 caps **for now**, and expects them to move to a **settings gear** later. That gear **does not exist in this repository yet** — `gears/simple-user-settings` is *not* it and must not be read as the target: its rows are keyed `(tenant_id, user_id)`, so there is no per-tenant row for a tenant-wide cap at all, it ships two fixed columns (`theme`, `language`), and its own PRD puts settings validation schemas and versioning out of scope. So the destination has to be **built**, and would need a per-**tenant** scope, a typed schema for policy entries (a cap is a bounded integer, the required-set an additive key list), and the authoring authz + audit these reads inherit here from §2.2 / `inst-rb-pep`. Until it exists this bullet is where an implementer finds them, and it is a resting place rather than the argument that a per-tenant cap belongs in a pricing table: build against this carrier, and expect the move.
- **`pricing_operator_flag`** — operator-plane drift/divergence flags, keyed `(tenant_id, subject_ref, flag)`; set/cleared by the external-signal handlers (audited): `tier_divergent` (Slice 2), `grants_divergent` (Slice 6), the tax-readiness divergence (Slice 4), `meter_binding_divergent` (Slice 2 — a registry metering-unit binding/dimension-set change diverging from a published plan's frozen mapping; 2026-07-31 review fix). **Never part of `pricing_read_model`** (D-85): a drift flag has no publish unit — consumers keep resolving the frozen values; operators read the flags via the authoring surfaces (`plan × read`) and the existing alarms.
- **`pricing_idempotency_dedup`** — PK `(tenant_id, operation, client_key)`, a request-payload digest, and the two response columns (`response_status`, `response_body`); the at-most-once gate + replay-response source; the idempotency check precedes the ETag check. **The row has exactly two states, and the TTL is evaluated at claim time (normative, D-142, 2026-08-02, found while building the draft-authoring plane).** The shape this bullet described — a hash beside a stored response — could not represent the instant the gate actually needs: the at-most-once gate **is** the primary-key INSERT, so the row must exist *before* the guarded operation has produced any response, and the only way to force the old shape was to seed a fabricated status into a column whose meaning is "this is what the caller was told" when nobody had been told anything. Five clauses. **(1)** The row is `claimed` (both response columns null) or `answered` (both set) and nothing else, with `(response_status IS NULL) = (response_body IS NULL)` enforced **physically** on both backends; the claim INSERT is the gate and precedes the guarded operation, and no synthetic response is ever seeded. **(2)** Expiry is evaluated **at claim time**, against the row as read: there is no reaper, and nothing in this gear deletes a dedup row — the store's **retention** is deliberately not decided here and stands as an open fork in the register; answering it cannot disturb this clause either way, since a row that has vanished is indistinguishable from a key never claimed and the INSERT path simply wins. **(3)** Expiry is evaluated **before** the payload-digest comparison; the reverse order hands the first payload to touch a key ownership of it forever, which makes the ratified 24h TTL (§1.2) unreachable and therefore not a bound at all. **(4)** A takeover of an expired row is a **compare-and-swap on the row as read**, so two racing takeovers cannot both win; the loser claimed nothing and executes nothing. **(5)** `record_response` is **write-once**: a second answer against an `answered` claim is neither an error nor an overwrite but the **replay path**, returning the **stored** response — exactly what `fr-mutation-idempotency` promises a retry, and what keeps an ordinary retry of a request that both exists and succeeded from reaching the caller as a not-found refusal. Rejected: a **reaper** as the expiry mechanism, which turns expiry into a background-timing property that must then race the compare-and-swap; and **seeding a synthetic `202`** at claim, under which the replay path would serve a response the gear invented. **A duplicate therefore has three outcomes, not two (D-143, 2026-08-02, flagged for veto).** A replay with a matching digest returns the stored response; a mismatching digest is rejected with `IDEMPOTENCY_PAYLOAD_MISMATCH` (never replayed, never re-executed); and a duplicate arriving against a `claimed`, unanswered key is refused with `IDEMPOTENCY_KEY_IN_FLIGHT` (409, §3.3). The third outcome is reachable **with no contract violation by anyone** — it is clause (4)'s losing caller, holding a request against a key that is claimed and unanswered, whose payload may differ from the winner's so that neither of the first two promises holds either — which is why stating the one-transaction contract normatively cannot close it, and why the surface layer needed something to say. At-most-once is never violated on that path: the loser executes nothing. **The digest is taken over the request as this gear models it, not over the bytes it arrived in (normative, D-174, 2026-08-03, found while building the authoring REST surface).** "A request-payload digest" left the subject of the hash unstated while all three outcomes above turn on comparing it, and the two readings differ **on the wire**: over the received bytes, a retry that merely re-serializes the same request — a different client, a re-encoding proxy, any library whose member order is not stable — digests differently and is refused `IDEMPOTENCY_PAYLOAD_MISMATCH`, a 409 the caller cannot fix on the one call they are obliged to retry. The digest is therefore over a **canonical rendering of the parsed request**, with a deterministic member order, so that a mismatch means the caller changed the request and not that the encoder did. Two consequences are stated rather than discovered: a member this gear does **not model** sits outside the digest, so a retry that adds one replays — the correct answer for a field the gear ignores, and the same fact as not refusing a legitimate re-encoding — and every **map-valued** member of a request type must have a determinate order, the rendering being canonical only if it does (this set carries one such member today, the D-48 descriptor set's additional properties). Rejected: the byte digest, on the argument above; and closing the request types to unmodelled members, which is a different decision with a cost this set accepts nowhere else — §3.3's read-model contract is explicitly additive-only within a major version.
- **`pricing_outbox`** — the transactional event outbox (frozen event names, dedup/correlation keys, `(tenantId, aggregateId)` ordering — enforced by a **unique sequence per `(tenant_id, aggregate_id)`**, named here 2026-08-03 because the ordering had been asserted with nothing stated to hold it, and because it is one of the three per-aggregate serialization points `CONCURRENT_MUTATION` covers, §3.3/D-159). **The correlation key is the request's, and it is the same value the audit record of that mutation carries** (**D-178**, 2026-08-03: this bullet and `inst-au-complete` both *required* a correlation and neither named a producer, so the one join an auditor and an operator both need — this event, that record, one operator call — rested on nothing; the producer is the request-scoped value the HTTP edge establishes, minted there when nothing is propagated inbound, and an in-process producer such as the publish commit supplies its own). **The key is a `uuid` column and the value is minted here, not adopted from the wire (D-181, 2026-08-03):** the platform's inbound convention (`traceparent` → `x-trace-id` → `x-request-id`) is real and the gear already sits inside it, but a trace id names a distributed trace rather than one operator call, so consuming it would put the *caller's* identifier on the events this outbox emits to Tariffs/Rating/Subscriptions/Billing and make "these rows were one act" a property of the caller's instrumentation. The correlation key stays this gear's own; the dedup key beside it is the consumer-facing one, and neither is the client `Idempotency-Key`.
- **`pricing_audit_log`** — append-only actor/before-after/approval trail, hash-chained per D-14 and **segmented per `(tenant_id, chain_id)`** with a periodic per-tenant roll-up chaining the segment heads (**D-135**, 2026-08-01 review fix — `chain_id` = the audited subject's aggregate: plan, overlay, payer, policy, bulk operation; a single per-tenant chain serialized *every* mutation of a tenant behind one head, inside the mutation transaction, which the ≥ 50 rows/s repricing SLO never accounted for); normative: [`05-governance.md`](./05-governance.md); ≥ 7-year configurable retention. **Its two discriminators have a declared vocabulary, and `subject_kind` is `pricing_approval`'s enumeration verbatim** (**D-158**, 2026-08-03, found while writing the table's first writer — the two stores discriminate the same aggregates for the same audience, and D-135 already keys the chain on the audited subject's aggregate, so a second vocabulary would let the approval record and the audit record of one decision disagree on what the decision was about); `action` is an additive `snake_case` verb set, never a frozen event name. Both are enumerated in [`05-governance.md`](./05-governance.md) §6, which owns the writer contract, and the `action` set gained the three **draft-authoring** verbs `create` / `update` / `delete` there (**D-175**, 2026-08-03 — every mutating authoring surface owes a record apiece and the opening enumeration named one of them (this said "the six" until 2026-08-17; §6 owns the roster and now states the class rather than a count, for the reason given there), so the set is now closed by the mutations this design set specifies rather than by that list: *no writer without a token*, the companion of D-158's *no token without a writer*), and the **approval plane's** two verbs `submit` / `withdraw` there as well (**D-180**, 2026-08-03 — applying D-175's closure rule to the approval store rather than to the authoring plane it was written against: `submit` is the record of the change unit §4's initial state opens, written by the non-committing arm of the publish route on `dod-audit`'s ground alone, and `withdraw` is the record of the route `inst-as-void` specifies and calls audited; the **machine-driven** TOCTOU void writes none, so an absent record against a `voided` unit is what says the guard closed it). **A same-segment race is `CONCURRENT_MUTATION` (409, §3.3, D-159):** the segment head is `MAX(seq)` under the primary key `(tenant_id, chain_id, seq)`, so two mutations of one aggregate that read the same head cannot both insert after it — the loser's whole mutation transaction rolls back, which is contention and not a fault of the store.

~~**Governed backdated reference rows are NOT in `pricing_price` (D-76).**~~ **Struck by D-330**
(2026-08-16). Historical import is out of scope, so the disjoint Slice-5-owned reference store
this paragraph carved out — never window-linked, never projected, never an input to coverage,
sellability or `CatalogVersion` addressability — leaves the design set with the flow that wrote
it. **Its conclusion survives and is now unconditional**: every statement in this design about
published `pricing_price` rows — §4.3 immutability, the scope-key partial `UNIQUE`, the
REVOKE/trigger discipline, "consumers resolve only committed versions" — holds **without an
exception class**, which D-76 had to build a second store to achieve and which is now simply true
of the only price plane there is.

**The physical enforcement this section claims is now proved by execution on the backend it
targets (recorded 2026-08-03, no D-number — a fact about the evidence, not a change to any
rule).** Every `CHECK`, trigger and partial index above had been verified by *reading the
statement text* beside an executed `SQLite` mirror, which proves the two branches say the same
thing and proves nothing about whether either is accepted by a Postgres server or refuses what it
claims to. The phase's retrospective sweep ran the migration chain **through the runner the gear
boots with** and then executed, for each object, the statement it must refuse. At phase close the
rosters stand at **68** `CHECK` constraints, **10** trigger functions behind **10** triggers and
**8** partial indexes, each with an executed refusal, across **171** Postgres tests. (The sweep
itself reported 62/9/9/8; the approval store landed later in the same phase and carries the
difference — six `CHECK`s, one trigger and its function — under the same discipline.) Three rules
make that evidence rather than coverage, and they are
recorded here because a later slice adding a table inherits them: a constraint is proved by the
statement it refuses and never by a valid write, which is the only way to catch a guard that
stopped refusing; the world is staged so the object under test is what answers, no neighbour
refusing first; and the rosters are pinned **by name** rather than by count, because a count of 68
is satisfied by 68 tautologies. Presence and refusal stay two suites: the chain-application suite
issues no DML and is evidence of presence only.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-pricing-deployment-fnd`

Stateless authoring/publish + read-model service over a shared `toolkit-db` backend;
background work (read-model warm re-drive) is coordinated as a singleton via the coordination
lease library. The read path is served from the projected read model and fails closed on
outage. Deployment specifics are platform-standard for a BSS gear, except the residency
constraint (`cpt-cf-bss-pricing-nfr-data-residency`): a residency-bound tenant's gear-owned
stores — `pricing_*` tables (incl. `pricing_audit_log`), read-model projection, outbox,
backups/DR replicas — are pinned to an in-jurisdiction deployment cell; the gear itself never
replicates across cells, and a residency-bound tenant configured onto a non-compliant cell
fails deployment config validation (fail-closed).

## 4. Additional Context

### 4.1 Canonical Scope Key (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-normative-scope-key`

The single scope key for **row-uniqueness, supersession, `PriceWindow` non-overlap, and window
coverage** is:

```text
(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)
  + on chargeKind = usage: (meter, dimensionKey)
```

**The usage pair is an axis of the key, conditionally (normative, D-196, decided by the product
owner 2026-08-06).** Every usage line of one plan in one `(currency, region, phase,
priceEligibility, cohort)` shares `chargeKind = 'usage'`, so under the eight axes alone they
rendered **one** key and the second line was refused `DUPLICATE_SCOPE_KEY` at authoring — which
made D-103's confirmed worked example (*"a PaaS plan pricing cloudlets, storage and egress is one
plan, not three"*) unstorable, while the meter-line index (§3.7), the publish pipeline's
meter-injectivity rule and S2's per-line completeness rule all presumed it storable. The pair is
therefore part of the key **on usage rows**, and the rule is an implication rather than a
biconditional: `meter` or `dimensionKey` present **implies** `chargeKind = 'usage'`, and the
converse is deliberately not asserted — a usage row carrying no meter is admitted today, the
meter/usage-type binding being registry-dependent and deferred (`inst-cmp-usagetype`), so the
converse would refuse rows the authoring plane accepts. **A `dimensionKey` with no `meter` is
refused, on a usage row as well (amendment, 2026-08-06, found while building the rule's code):**
a dimension discriminates the dimensions *of* a meter, so with no meter it names nothing — and
admitting it would hand the store a second key for one meterless usage line, which is the
duplicate this pair exists to prevent. Both refusals report `USAGE_LINE_AXIS_MISMATCH`, the code
this rule owns for `COHORT_ELIGIBILITY_MISMATCH`'s reason: a publish-blocking rule with no code
cannot be reported to the operator who has to fix it. **The line is authored on the row and the axes are derived from it (normative, D-196 clause 3,
2026-08-06).** Each axis of this key is stated by whichever half of a request can carry it, and
for this pair that is the **content**: `chargeKind` is expressible only on the key, so a row's
copy of it is rewritten *from* the key; `meter` and `dimensionKey` are expressible only on the
row, so the key's ninth and tenth axes are derived *from* the row. Neither direction is a
preference — in each case the other half has nothing to say. Two consequences follow and both
are rules rather than incidents. **An authored row whose own line differs from a line its key
already names is refused**, never silently reconciled, because a door that picked a winner would
make the S3 supersession unit guard's `meter` and `dimensionKey` clauses unreachable — they
compare the two rows, and two of the four components of the tier counter's key live there. **And
an update may not move the line**, exactly as it may not move any other axis: the remedy is the
one §4.3 already names for a key, delete the draft and author another. **The rendering carries
ten segments always**, `none` in both positions on a non-usage row: the rendered key is embedded in the
`DUPLICATE_SCOPE_KEY` message, in the approval register's held-key rows and in the registry
idempotency key, so its arity has to be fixed rather than a function of the row. The physical
half — how a nullable `meter` is kept from dissolving the uniqueness the two partial `UNIQUE`s
already have — is §3.7's.

**Every site that compares, renders, hashes or stores this key compares ten axes (D-205,
2026-08-06).** Recorded here rather than left to each implementation, because widening a key is
not a schema change: it **re-classifies fields**, and the implementation found four sites still
at eight, each by a different route and none by any gate — a row-pairing guard, the sellability
gate's sibling comparison (which dropped a bound key from the gate and answered over a market
with no window), the approval content pin (live wherever a key is pinned with no row beside it,
masked wherever a row repeats the same two values), and the served rendering (two lines answering
as one `scope_key` object). D-205 also records the mechanical lesson: a comparison defined by the
axes it *excludes* cannot be stated at a distance, because nothing tells it the list grew.

- Axis defaults: `priceOverlay = base` (rows authored here always carry `base`; partner/orgTier/brand overlays are separate `PriceOverlay` rows evaluated downstream by Tariffs), `phase =` **the plan's terminal `phase_id`** (D-19: the axis is always uuid-typed — for a phased plan its authored terminal phase; for non-phased/one-time plans an **implicit terminal phase row** (kind `evergreen`) is auto-created at plan creation and its id is the default; the literal `evergreen` survives only as the phase *kind*), `priceEligibility = all_subscriptions`, `chargeKind` per row, `cohort = none`.
- `cohort` (ADR-0002) is the **grandfathering generation discriminator** — the UTC cutover instant that created the generation, **at the millisecond quantum §2.2 fixes** (D-144, 2026-08-02): this axis is matched for *equality*, so its resolution is part of the key's meaning and not a rendering detail. Publish validation enforces `cohort ≠ none ⇔ priceEligibility = existing_grandfathered` (`COHORT_ELIGIBILITY_MISMATCH` — the rule was normative from the start and had no code of its own; named here 2026-08-02 while implementing the axis, since every other publish-blocking rule in this set carries one and a rule with no code cannot be reported); every cutover creates a **new** generation on its own key, so repeated repricing with per-cohort retention never violates non-overlap. Within the `existing_grandfathered` class, Tariffs selects the row whose `cohort` equals the cohort of the subscription's **pinned price id** (`pricingSnapshotRef`) — and, when that pin carries `cohort = none` (the **bootstrap** case, D-126: the subscription predates the key's first cutover, so its pin is on a non-grandfathered row), the generation whose `cohort` equals the **pinned row's window `effectiveTo`**, which is by construction the instant of the cutover that closed it — **the equality is at the millisecond quantum on both sides** (D-144), which this comparison in particular depends on, since the two instants are produced by different code paths in different gears and two instants denoting the same moment at different resolutions are not equal; if no generation carries that instant the class contributes **no** candidate and resolution continues down the class order (the pinned row was closed by a supersession, not a cutover — that subscriber is not grandfathered). Class ordering (most-specific-wins) is unchanged. Unrelated to `customerGroup` segment pricing.
- `chargeKind ∈ {recurring, usage, one_time, one_time_setup}` distinguishes the components a single plan legitimately carries at once: a hybrid plan holds a `recurring` **and** a `usage` row (optionally a `one_time_setup` row) on one `planId`, and a one-time plan's base row is `one_time` — so they are **distinct keys**, not duplicates.
- `brand` is **NOT** a price-row axis: brand-differentiated pricing is a **brand-scoped `PriceOverlay`** overlay (manifest §4.1 invariant).
- This key **extends the manifest `(plan, currency, region, priceOverlay)` key additively** with `phase`, `priceEligibility`, `chargeKind` (ADR `cpt-cf-bss-pricing-adr-canonical-scope-key`) and `cohort` (ADR `cpt-cf-bss-pricing-adr-grandfathering-cohort-axis`), and **supersedes** the narrower effective-dating `(plan, currency, region, priceOverlay)` key for normative purposes.

Because `priceEligibility` and `cohort` are part of the key, a grandfathered generation and
its successor — and any number of prior generations — are **distinct keys** that hold active
windows concurrently at the same instant without violating non-overlap (§4.3).

**`grandfatherUntil` is a grandfathered-row field (normative, D-147, 2026-08-02, found while
building the draft-authoring plane).** `grandfather_until` is non-null **only** on a row whose
`priceEligibility = existing_grandfathered`; a value anywhere else fails publish
(`GRANDFATHER_UNTIL_FORBIDDEN`, 422 — declared by the slice that owns the eligibility machinery,
[`07-pricewindow-linkage.md`](./07-pricewindow-linkage.md) §5). It is the `cohort` rule's sibling
and takes the same shape — one axis-conditioned field, one code — and it is stated here because
until now the pairing was enforced by a column check and by nothing else: §4.3's
only-permitted-mutation clause, `inst-gs-bound`/`inst-gs-tighten` and `inst-cl-resets` (which
resets the two together) all read as if the rule held, while `inst-el-fields` publishes
`grandfatherUntil` "per row" unqualified and the PRD glossary lists it among a **Price row**'s
optional fields. A constraint with no rule gets no code, so its violation reached the caller as an
internal fault — a 500 for a request the operator could have reshaped. Rejected: reading
`grandfatherUntil` as a **general per-row availability bound**, which the unqualified wording
admits. The set already carries two mechanisms for "this row stops being sellable at `T`" — the
window's `effectiveTo` and the plan's `available_to` — and a third that only the eligibility
machinery derives a signal from (`inst-gs-expire`'s read-derived `EligibilityExpirySignal`, whose
entire meaning is "re-bind at the next renewal") would give one fact three unreconciled homes.
The converse is deliberately **not** stated: a grandfathered row with a null `grandfatherUntil`
is indefinite.

### 4.2 Publish-Through-The-Engine Contract (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-normative-publish-contract`

Every state change that reaches production follows one path:

1. **Author draft** — create/update/clone in `draft` (ETag-checked, idempotency-deduped). Only `draft` rows are mutable; a never-published `draft` **price** row is deletable, while a plan's open `draft` **revision** row is abandoned rather than deleted (D-145, §4.3).
2. **Validate (fail closed)** — the aggregate validation pipeline runs the §17.4 rule set plus every slice-registered rule; a single failure blocks the submission with an enumerated report. This step is a **pre-check**: the same rule set re-runs inside the publish-commit transaction of step 4 (approval approves content; the commit re-validates state — a commit-time failure voids the approval and returns the subject to draft with the report). No `PlanPublished`, no read-model warm on any failure.
3. **Approve** — a material change (above the configured threshold, or a first publish) requires the submitter **plus ≥ 1 independent approver** (two distinct principals; self-approval rejected + audited). Fail-safe: the two-person rule applies unless an explicit threshold is configured and the change is below it and it is not a first publish. Pending approval is **not** a `lifecycle_state`: the subject stays `draft` and remains mutable — the open Slice 5 approval record marks it, and any mutation of the subject **voids** that record ("returns to draft" in the PRD means the approval record closes).
4. **Freeze + emit** — the publish commit re-runs the pipeline, **requests the `CatalogVersion`**, transitions the row set, and stamps the catalog-side `pricingSnapshotRef` identifiers; the frozen event set is enqueued transactionally (`PlanPublished` with a **pending** version ref). **The request is made inside this transaction, after the re-validation passes and before any write (normative, D-156, 2026-08-03, found while building the commit).** §3.6 had it after the enqueue, which cannot be built: at that point there is no handle for the payload to carry. Earlier than stated is worse than later — a commit-time validation failure would strand a pending ref that nothing ever commits, tripping `pricing.catalogversion.commit_overdue` (§3.6) for a publish that never happened. The cost is a **network round-trip inside an open database transaction**, and it is bounded by deriving the `request_id` from `(tenant_id, plan_id, revision)`, so a rolled-back-then-retried commit re-requests the *same* handle rather than orphaning one and any pending handle is attributable to the unit that asked for it; the one **permanent** post-request refusal — a retired predecessor, which no retry can ever make publishable — is therefore asked **before** the request. **The catalog-side stamp is all three segments (normative, D-162, 2026-08-03):** the pending version ref, the resolved price ids, and the **evaluation-policy generation** — the declared `ep-<n>` constant naming the evaluation-policy field set the frozen row content is to be read under, whose roster, bump rule and log are §4.4's. D-161 decided the half that could be decided before the segment had a producer at all — publish stamps a real value or **no placeholder** — and that ban is unchanged: Rating's `SnapshotComposer` fail-closes on a missing pricing pre-stamp, which is the correct guard, and a fabricated version *satisfies* that guard, is pinned immutably on posted periods, and cannot be told from a real one afterwards. §4.4 carries the same statement.
5. **Version + warm** — the registry (sole incrementer) batches approved publishes and emits `CatalogVersionPublished`; the read model warms to the 5s SLO or the publish is marked degraded (`PlanPublishDegraded`). No intermediate state exposes a rateable-but-incomplete plan.

**The commit flips exactly the row set its re-validation judged (normative, D-155, 2026-08-03,
found while building the publish commit).** Step 2 says the rule set re-runs inside the commit
transaction and step 4 says the commit transitions "the row set" — and until this rule nothing
said **which** row set, so re-reading the plan's `draft` rows at flip time was a conforming
reading of both. It is the wrong one. The transaction runs at the engine's default isolation
(every statement takes a fresh snapshot), and the window between the assembly the rules judge
and the flip contains the step-4 registry round-trip: in it, a concurrent draft creation
publishes a row **no validator ever saw**, and a concurrent draft edit publishes a mutation of
a row that had passed. Three clauses close it.

1. **The subject is pinned by identity and version.** The flip applies to exactly the
   `(price_id, row_version)` pairs the re-run judged. A row whose version moved is refused
   naming the row (`STALE_VERSION`, §3.3 — **no new code**; this is what D-141 gave every price
   row its own tag for); a row that did not exist at assembly is not in the set, stays `draft`,
   and publishes with the next revision. The **pre-check** deliberately pins nothing — a row
   authored between pre-check and commit *is* judged, by the second run — so this is a property
   of the commit transaction alone.
2. **Every other input the rule set reads is held by a named mechanism, and the list is
   closed.** The draft revision row: its own compare-and-swap. Its Slice-2 children (phases,
   add-on rules, descriptor set): that same revision tag, which every child edit bumps. The
   plan's **published** price rows' *content*: `pricing_price`'s append-only trigger. The
   current published revision row: `pricing_plan`'s frozen-column whitelist. That revision's
   phase set: `pricing_plan_phase`'s append-only trigger. The tenant policy row: **nothing** —
   a policy edit committing afterwards means the publish was judged against the snapshot it
   read, which is staleness rather than a bypass (the next publish enforces the new value), and
   this gear has no writer for that row. The list is written out because an unchecked "all" is
   what hid the defect above. **`SERIALIZABLE` is not required** and is declined on its merits:
   it buys nothing the tags and triggers already buy, and it would hold predicate locks across
   the step-4 round-trip, turning a slow remote service into blocked publishes. Note what the
   fork-freedom of the counters does **not** prove: `(tenant_id, chain_id, seq)`, the outbox
   sequence and `uq_pricing_plan_current` make their forks unrepresentable at any isolation
   level, but a key can only refuse a second row at a position it already knows about — **a
   conclusion about predicates does not follow from a premise about keys.**
3. **One entry in that list is a premise, and it is stated as one.** The append-only trigger
   guarantees a published row's *content*; it does not guarantee the *membership* of the
   published set, because the same trigger sanctions `published → superseded`. Four
   completeness rules range over that set (`inst-ph-coverage`, `inst-cs-hybrid`,
   `inst-cs-usage`, and D-84's per-market usage rule) and the in-use phase id set is derived
   from it, so a row leaving it between assembly and commit changes what was judged. What holds
   it **today** is that this gear has **no producer for that flip at all**: its two sanctioned
   producers are the **D-88** supersession unit and the **D-100** cutover, both Slice 7's, and
   neither is built. **The group that builds either deletes this premise**, and then owes the
   published half a membership guard — the same `(price_id, row_version)` shape extends to it,
   published rows' tags being frozen with their content (§3.7) — or must pin that membership
   another way. An unstated premise a later slice will delete is how this class of defect
   survives a review.

Publish units are not only plans: Slice 9's `PriceOverlays` and customer-group
membership mutations publish through the **same engine** (validation → pending ref → warm;
D-06) — nothing becomes consumer-visible outside a committed `CatalogVersion`.

**Plan retirement is a publish unit too (normative, D-128, 2026-08-01 review fix).** The
`published → retired` flip runs the same engine path (validation → pending `CatalogVersion`
ref → warm) and **re-projects the plan subject**, because the plan's `lifecycle_state` is
sellability predicate (4) ([`07-pricewindow-linkage.md`](./07-pricewindow-linkage.md)
`inst-sg-surface`) and that predicate is required to be evaluable from the *pinned* read model
(`inst-sg-pinned`). Before this rule retirement requested nothing and warmed nothing —
[`11-lifecycle.md`](./11-lifecycle.md) `inst-rt-event` discharged it as "the read model flags
the plan not-sellable", an in-place mutation of a frozen version that D-85/D-99 forbid, and
[`../PRD.md`](../PRD.md) §17.5's increment table listed no retirement class at all. It cannot
self-heal like other lagging facts: a retired plan can never publish again (no
`retired → draft`/`published` edge, and its open draft revision is **abandoned** in the same
transaction — **D-145**, 2026-08-02: the revision row is flipped to the terminal `abandoned`
state rather than deleted, so the number it consumed is never re-minted (§4.3), and the flip is
audited exactly as the deletion was; D-128's own three clauses are unchanged), so **no later
publish would ever re-project it** and the read model would
advertise a retired plan as sellable permanently. The projection source is correspondingly the
plan's **current** revision — `published` *or* `retired` (§4.4/§3.7) — so a retired plan keeps
a resolvable delta for the in-flight subscribers D-51 preserves coverage for.

**Window mutations are publish units too (normative, D-99, 2026-07-31 review fix).** Every
committed `WindowScheduler` mutation — schedule, future-`effectiveTo` adjustment, cancellation
([`07-pricewindow-linkage.md`](./07-pricewindow-linkage.md) §5) — runs the same engine path
(validation → pending `CatalogVersion` ref → warm) and **re-projects the affected rows' plan
subject**, because window facts are what the sellability gate's predicate (1) and the D-80
coverage horizon are evaluated from and those are required to be resolvable from the *pinned*
read model (`inst-sg-pinned`). Windows are plan facts, so they need no subject kind of their
own. Before this rule the window surface requested nothing and warmed nothing, while
[`../PRD.md`](../PRD.md) §17.5's increment table already required a window edit to become
addressable in a `CatalogVersion`: a cancellation left the last-warmed delta advertising
coverage the truth side had removed (selling into the trailing void D-62/D-80/D-94 close), and
a coverage extension could not lift a horizon block until some unrelated publish re-projected
the plan. The cutover and supersession units already requested addressability
(`inst-gc-commit`, `inst-su-commit`); this closes the standalone surface. Activation and
expiry, by contrast, are **not** publish units and need none — see §4.4: the read model carries
window **intervals**, and "active at `t`" is derived at read time.

Consumers never read draft state and never substitute a default for an absent
evaluation-policy field (absence must have failed step 2). The catalog computes **no** monetary
charge, evaluates **no** overlay, and performs **no** FX.

### 4.3 Immutability and Change Mechanisms (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-normative-immutability`

Published `pricing_price` rows are **append-only history**. A transition this section does not
sanction is refused with `LIFECYCLE_FORBIDDEN` (§3.3) — mutating a published row's content,
superseding a grandfathered row — and the refusal is enforced
twice, by the validation pipeline and by the physical guard below, because the engine is not
the only thing that can reach the table. `REVOKE` + a trigger with a
**column whitelist** reject any UPDATE except the state-machine `lifecycle_state` transitions
(`published → superseded`) and monotonic `grandfather_until` tightening — price/scope/model
columns **and the row-version column** are immutable and DELETE is always rejected (§3.7, which
states why the tag freezes with the content it names — D-141); only never-published `draft`
price rows are deletable (a plan's open draft **revision** row is abandoned instead, per the
revision rule below); there is no deletion event to fan out. Change over time uses four **distinct,
composable** mechanisms ([`../PRD.md`](../PRD.md) §17.5):

- **Versioning** — captures a structural/price change as a new immutable revision; prior rows retained as history (`PlanUpdated` / `PriceCreated`).
- **Supersession** — versioning scoped to **one canonical scope key**: a new immutable row plus opening/closing the corresponding `PriceWindow` (never overlap), within one `priceEligibility` class and one `chargeKind` — and, on usage rows, with the unit/counter-determining fields, **`model_kind`**, **`package_size`** and (on a `carry`-allowance row) **`included_allowance`** preserved (meter, granularities, aggregation windows; `SUPERSESSION_UNIT_MISMATCH` otherwise — D-82 + D-98 + D-122 + D-129, Slice 3 rule) (`PriceUpdated`). **The guard binds the *key*, not the mechanism (D-127, 2026-08-01 review fix):** it applies to **every** row that lands on an occupied published canonical scope key — the supersession unit **and the cutover's `all_subscriptions` successor**, which lands on the predecessor's own key (D-100) and inherits the same continued counter. Both sanctioned producers set `supersedes_price_id` on the successor, which is what gives the guard its comparison referent. It executes as the **supersession unit** (D-88, Slice 7): successor row + predecessor-window shorten + successor-window schedule composed gap-free, one approval unit, one local ACID commit — the only way to supersede *interactively*; the grandfathering cutover is the **second** sanctioned producer of the same flip (D-100, below). The predecessor's `lifecycle_state` flips `published → superseded` at the supersession **commit**, not at window activation — effectiveness over time lives solely in the `PriceWindow`, so the **published-plane** scope-key partial `UNIQUE` (§3.7 — the draft-plane index D-148 adds is a disjoint second one) always sees exactly one current row per key (2026-07-28 review fix, confirmed 2026-07-31).
- **`PriceWindow`** — schedules **when** a versioned/superseded row is effective (window store, state machine, and activation job owned by Slice 7 in this gear — D-03).
- **Grandfathering cutover** — one atomic approval unit that shortens the current `all_subscriptions` window `effectiveTo` to the cutover and schedules (a) an immutable `existing_grandfathered` copy and (b) the `all_subscriptions` successor, so **no coverage gap opens**. Because the successor lands on the **same** canonical scope key as the predecessor (only the copy moves to a new `cohort` key), the cutover commit **also flips the predecessor `published → superseded`** — the second sanctioned producer of that transition beside the supersession unit (**D-100**, 2026-07-31 review fix: `algo-cutover` had enumerated its whole transaction without the flip while the price-row state machine called the supersession unit the *only* path, so the commit inserted a second published row on an occupied key and died on the published-plane scope-key partial `UNIQUE`). The successor therefore carries `supersedes_price_id` and passes the **same unit guard** as an interactive supersession (**D-127**, 2026-08-01 review fix: the guard was written as "a *usage-row supersession*" and invoked only from `inst-su-compose`, so a cutover successor could flip `per_hour → per_day`, `graduated → volume` or `package_size` on a key whose counter continues — the ×24 class through its fifth door, on the one path that is always material and therefore felt safe).

**Plan revisions (D-56, 2026-07-28 review fix, confirmed 2026-07-31; child tables per D-83):**
`pricing_plan` is keyed
`(plan_id, revision)`. A published revision row is **immutable in content** (its only in-place
mutations are the sanctioned `lifecycle_state` flips: `published → superseded` when its successor
revision's publish commits — D-90, so at most one revision per plan is ever **current** (partial
`UNIQUE … WHERE lifecycle_state IN ('published', 'retired')`, widened by D-128) and "the current
revision" is storage-defined, never a max-scan convention — and
`published → retired` at retire, itself a publish unit per §4.2); a shape change on a published
plan opens a **new** revision row in `draft` (at most one open draft per plan — partial
`UNIQUE (plan_id) WHERE lifecycle_state = 'draft'`) that publishes through the standard §4.2
path and becomes the current revision, flipping its predecessor `superseded` in the same commit. The plan's identity, the scope-key axes, and the
`pricing_price` attachment stay on `plan_id` (unchanged), and the child shape tables — phases,
add-on rules, descriptor set — **version with the revision, copy-on-new-revision** (D-83,
2026-07-30 review fix): the new revision copies them with stable `phase_id`s (so the `phase`
scope-key axis and same-key supersession are untouched), the open draft edits **its own
copies**, and a published revision's child rows are immutable with it
([`02-plan-definition.md`](./02-plan-definition.md) §6) —
so "published plans never return to draft" means the published revision row and its child rows
never mutate; change is always a new revision row.

**`revision` is an identity, never re-used (normative, D-145, 2026-08-02, found while building
the draft-authoring plane).** A number minted for a plan is never minted again. An open draft
revision that is discarded — by its author, or by the retirement transaction that closes it
(§4.2; [`11-lifecycle.md`](./11-lifecycle.md) `inst-rt-cancel`,
[`02-plan-definition.md`](./02-plan-definition.md) `inst-pl-abandon`) — is **not deleted** but
flipped to the terminal `abandoned` `lifecycle_state`: its child copies are dropped, the flip is
audited exactly as the deletion was, and the row survives as a tombstone, so minting is
`max(revision) + 1` over the plan's own rows and consults nothing else. Deletion freed the
number, and `(plan_id, revision)` is a **durable name** this set already dereferences —
`pricing_plan_grant` is keyed `(grant_id, plan_revision)` (D-52/D-106), every revision-scoped
child table copies on new revision (D-83/D-92/D-106), and the audit trail records the revision it
mutated — so `plan/2` could name two distinct rows over a plan's lifetime. Nothing observes that
while the current revision never regresses, but the plan row's ETag does: a client holding the
discarded `plan/2`'s row version `PATCH`es the **new** `plan/2` believing it is editing the old
one and the precondition **passes**, most obviously at the initial version every freshly minted
revision carries. That is the lost update optimistic concurrency exists to refuse, arriving
through the key instead of the version. `abandoned` is terminal and sits outside **both** partial
`UNIQUE` predicates (§3.7) — outside `= 'draft'`, so a replacement draft opens immediately **on a
plan that has published at least once** (the never-published plan is the exception, and the
amendment below states it), and
outside `IN ('published', 'retired')`, so "the current revision" (D-90, widened by D-128) is
untouched. Consequence, stated plainly: a plan's revision numbers may have **gaps** (rev 1
published, rev 2 abandoned, rev 3 published), which the Slice-12 history surface shows an
operator. Rejected: (a) keeping deletion and forbidding `(plan_id, revision)` as a reference —
refuted by the set itself, since `pricing_plan_grant`'s primary key *is* that reference; (b)
keeping deletion and sourcing the next number from the audit log, which puts an append-only
forensic store on the authoring path. The scope is the **plan revision row**: price-row
deletability is untouched, so the price-row clause above and
[`03-price-structure.md`](./03-price-structure.md) `inst-ps-nodelete` keep their meaning.

**A plan that never published spends its id when its one draft is abandoned (normative, D-145 as
amended 2026-08-02).** "A replacement draft opens immediately" is a statement about the partial
`UNIQUE` index, and the index is not the only thing a new revision has to get past. Minting has
**two** entry points and only one of them is `max(revision) + 1`: a plan's **first** draft is
minted at revision `0` outright — there is no row to take a maximum over — while opening a
**successor** presupposes a current revision to succeed from. So the one plan the rule above does
not cover is the plan created and abandoned before its first publish: it holds exactly one row,
revision `0`, `abandoned`; it has no current revision and no open draft; and it can acquire
neither, because creating collides on the `(plan_id, revision)` primary key and opening a
successor finds nothing to succeed. The `revision` identity rule is what makes that true — before
it, deletion freed revision `0` and a retry worked — and it is **kept**, because the two ways out
are both worse. Minting `max(revision) + 1` on the create path as well would close the hole and
make a retried create naming the same id open a **second revision of an existing plan** instead of
refusing it, and refusing is what creation idempotency rests on (§3.7 `pricing_idempotency_dedup`,
[`../PRD.md`](../PRD.md) `fr-mutation-idempotency`). Exempting revision `0` from the identity rule
would let `plan/0` name two rows over a plan's lifetime — the unstable reference this whole rule
removes, reintroduced on the one number every plan starts at. A plan id is minted server-side, so
a spent id costs an operator nothing; a silent second revision costs a great deal.

**What is owed is the answer, not the state.** An authoring call naming such a plan is refused
with `PLAN_ABANDONED_NO_SUCCESSOR` (422, §3.3) — a **precondition refusal that names the state**,
where the storage layer alone produces a not-found (no current revision to succeed), which does
not tell a caller that the id is permanently unusable and that retrying is pointless. **Three
surfaces owe it and the create is not among them (D-172, 2026-08-03, found while building the
authoring REST surface):** `PATCH …/plans/{planId}`, `POST …/plans/{planId}/publish` and
`POST …/plans/{planId}/abandon`. The first draft of this paragraph named a fourth,
`POST /bss-pricing/v1/plans` "retried with that id", and that arm presupposed a **caller-supplied**
plan id, which the sentence above rules out in as many words — a plan id is minted server-side, so
a retried create carries an `Idempotency-Key` and no plan id at all, and is answered by the replay
path (§3.7). The primary-key collision it described is real in storage and reachable by no caller.
A **`GET`** of such a plan is not a fourth arm either: it answers **404**, the gear's ordinary
absent-or-out-of-scope answer, because the route serves the plan's *editable* revision (D-170) and
there is none — what is absent is the representation this route offers, not the plan, whose
abandoned revision is a Slice-12 history subject. It is a code of its own rather
than `PLAN_RETIRED_NO_SUCCESSOR`, which would assert a retirement that never happened over a plan
that still has a current revision, a warm delta and a clone route forward, and rather than
`LIFECYCLE_FORBIDDEN`, which D-146 leaves holding exactly the refusals with **no alternative
action to describe** — this one has one, and it is specific: the id is spent, mint a new plan.
**The refusal is raised** (2026-08-03): the authoring REST surface this paragraph recorded as
absent has landed on `bss/pricing-impl` as Group **G7**, and it discriminates the spent plan on
all three arms above, in process and on the wire; the repositories underneath are unchanged, as
this paragraph said they should be.

**Two refusals were narrowed out of `LIFECYCLE_FORBIDDEN` (normative, D-146, 2026-08-02, found
while building the draft-authoring plane).** Opening a revision on, or re-publishing, a
**retired** plan is `PLAN_RETIRED_NO_SUCCESSOR` (422); a second draft revision on a plan that
already holds one is `OPEN_DRAFT_REVISION_EXISTS` (409, naming the open revision). Both are
**narrowings**, not codes added beside `LIFECYCLE_FORBIDDEN`, so no refusal ends up with two
codes. A consumer must be able to tell them apart without parsing prose, because the operator's
next action differs: a retired plan can never publish again, so any successor is unpublishable by
construction and there is nothing to do, while "you already have an open draft at revision N"
names a **different and available** action — go and edit it. The second is not a state-machine
transition at all but a uniqueness conflict on the `(plan_id) WHERE lifecycle_state = 'draft'`
partial index (§3.7), so even the 422 category was a compromise; 409 puts it in the
`DUPLICATE_SCOPE_KEY` class that §3.3's status rule keeps out of the 400 bucket. What stays under
`LIFECYCLE_FORBIDDEN` is exactly the refusals with no alternative action to describe — mutating a
published row's content, superseding a grandfathered row. Rejected: giving **every** internal
refusal its own code. The read model's forward-only pin frontier (D-136, §4.4) draws the line: it
is advanced only by the ReadModelProjector and is unreachable by a caller, so a regression there
is an internal fault rather than a lifecycle refusal, and a wire code for it would document an
API a client cannot provoke. Both codes are Foundation-owned; slices reference and never redefine
them (§3.3).

An `existing_grandfathered` row is **immutable in price** and MUST NOT be superseded; the only
permitted mutation is **setting or tightening `grandfatherUntil`** (never loosening, never the
price), which is a material change. Because it is a distinct scope key (via `priceEligibility`),
it holds an active window concurrently with its successor and is **live-resolved** by Tariffs
against an immutable row — reconciling live resolution with the frozen-snapshot doctrine.

### 4.4 Read Model and pricingSnapshotRef (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-normative-read-model`

The published read model is **monotonic per `CatalogVersion`**: a subject's row at a version is
ignored until `CatalogVersionPublished` **and** that row's `warm_completed` marker are both
present — the marker is stored **per subject row** (D-86/D-91).

**Pin-eligibility is version-level and prefix-closed (normative, D-101 + D-114, 2026-07-31
review fixes; amends D-91's resolution rule, not its storage).** A `CatalogVersion` becomes
**pin-eligible** only once `CatalogVersionPublished` has been emitted, **every subject row that
version projects carries its `warm_completed` marker**, **and every earlier version is itself
pin-eligible** (D-114) — pin-eligibility is a **monotonic frontier**, and "the newest completed
version" — the referent of the ≤ 5s pin-lag rule below — means that frontier's edge.

**Both quantifiers are this tenant's (normative, D-164, 2026-08-03, found while building the
read side).** `CatalogVersion` is minted by a **cross-tenant** registry, so a tenant's committed
versions are a sparse subset of the global sequence: read globally, "every earlier version is
itself pin-eligible" is a condition no tenant can ever satisfy — every frontier in a deployment
sticks at the first version another tenant's publish consumed — and "every subject row that
version projects" would make one tenant's unwarm subject hold another tenant's pin. So the
prefix is over **this tenant's committed refs in version order**, and the subject set is **this
tenant's subjects of that version**, which is what `pricing_pin_frontier` being keyed
`tenant_id` alone already assumed and what nothing had said.

**A version's subject set is closed by its first committed ref (normative, D-163).** That
follows from batch atomicity — one batch, one version, one event (§3.6) — and it is the premise
the whole predicate rests on: without it "every subject row that version projects" is a set
still growing while it is being counted. The projector therefore decides completeness only from
a pass whose knowledge covers that set, and refuses a ref resolving into a version at or below
the frontier; both are stated with D-163.

Resolution
stays per subject (greatest completed version ≤ the pin), and under the two rules together the
per-subject fallback is **never observable at a pinned version**: every subject the pin's
version touched is warm at it, and every older delta a fallback can reach lies at or below the
frontier, so it is complete and frozen. Each condition closes a real divergence. Without
version-level eligibility (D-101) a *single* pin resolved two different contents over time —
plan `P` at `V-3` before its delta warmed and at `V` after. Without the prefix (D-114) the same
divergence survived one version out: with `V5` degraded (plan `A`'s warm outstanding — a
re-drive continues past the SLO with no bound, §1.2/§3.6) and `V6` fully warm, `V6` was
pin-eligible while a pin of it resolved `A@V4` — and resolved `A@V5` once the late warm landed,
because the fallback reaches **through** the pin into older deltas that nothing else required to
be complete. A version that has not become pin-eligible within the max batching-delay SLO
raises Critical (`pricing.readmodel.pin_eligibility_overdue`) and rides the existing degraded
path — a stuck version now holds the frontier, which is exactly what that alarm signals;
consumers meanwhile keep pinning the frontier's previous edge and resolve it **coherently**
(uniformly pre-change), never a mixed set.

**The frontier is materialized, not recomputed (normative, D-136, 2026-08-01 review fix).**
Pin-eligibility as stated above is a predicate over *every* subject row of *every* version, and
its prefix clause makes it recursive — evaluated literally on the read path it is a scan of the
delta store on a p95 < 100 ms budget, and the `pricing.readmodel.pin_eligibility_overdue` alarm
has nothing to evaluate at all. Therefore the frontier is a **stored per-tenant watermark**,
`pricing_pin_frontier(tenant_id, catalog_version, advanced_at)` (§3.7): the ReadModelProjector
advances it **in the same transaction** that sets the last outstanding `warm_completed` marker
of the frontier's next version in order, and only ever forward (a later version's completion
never advances it past a gap — that is the D-114 prefix, enforced by construction rather than
re-derived). It is the **only** definition of "the newest pin-eligible version": consumers read
it from the read-model contract (§3.3) and pin its value, the ≤ 5s lag rule is measured against
it, and the overdue alarm fires on its age. Nothing recomputes the predicate at read time.

**The advance then walks forward (normative, D-164).** Read literally, the sentence above
advances the frontier only in the transaction completing the frontier's **next** version, which
strands a version that completed out of order: this section's own D-114 worked example is `V5`
degraded and `V6` fully warm, and when `V5` finally completes the frontier moves to `V5` while
`V6` — already complete — never sees another completion to advance on and stands behind the
frontier forever. So after an advance the projector walks the watermark through every
immediately-following version of this tenant that is **already** complete. Every step is still
"the frontier's next version in order" and still complete, so the walk is strictly forward and
never past a gap; what it is not is the literal sentence.

**When the projector may call a version complete, and what it does when it cannot (normative,
D-163).** Batch atomicity closes a version's subject set at its first committed ref, but the
projector learns each ref's version by asking the registry **per ref**, so its knowledge of that
set is only as complete as the pass that gathered it. It may therefore decide a version complete
**only from a pass that could have seen the whole set**, and two conditions say it could not:
the pass's pending scan reached its bound (a batch straddling the page boundary is split across
passes, and the first pass would see a partial subject set), or a ref of the same tenant failed
to resolve (its sibling completes the version while the errored ref is still to arrive). In
either case the pass resolves and warms exactly as usual and decides **no** completion; the
frontier lags rather than over-advances, which is the safe direction and is visible as the
overdue alarm below. The pending scan is bounded **per tenant** for the same reason: on a
cross-tenant page one tenant's stuck backlog defers every other tenant's completions, and the
deferral this rule introduces has to be a tenant's own. **A ref that resolves into a version at
or below the frontier is refused** — an already-pin-eligible version acquiring a new subject is
precisely one pin resolving two contents over time. Under the rules above it is unreachable on
a correct path, so it detects a broken batch-atomicity contract rather than predicting one, and
it costs that publish permanently: its subject is never projected, the alarms fire every pass,
and the remedy is §3.6's out-of-band registry re-request. **No wire code:** the projector has no
caller, so a refusal on it has nobody to report to (D-146's own line about this frontier).

A consumer pins **one** pin-eligible `CatalogVersion` for the duration of a resolution/rating
run and resolves the complete frozen view via `pricingSnapshotRef`; at pin time the pinned
version MUST NOT lag the frontier by more than 5s. There is **no** draft read
and **no** default substitution.

**Window facts are projected as intervals (normative, D-99; projected set + horizon per
D-121).** A plan subject's row carries, per canonical scope key, the key's `PriceWindow`
**intervals** and states (`[effectiveFrom, effectiveTo)` + `scheduled | active | expired`;
cancelled windows are not projected) and the derived **coverage end** — never a point-in-time
"is active" boolean. "Active at `t`" and the D-80 coverage horizon are therefore evaluated **at
read time** against frozen intervals, which is what makes the six sellability predicates
point-in-time evaluable from a pinned version (`inst-sg-pinned`) *and* what makes the
time-driven `scheduled → active` / `active → expired` transitions need no re-projection (a
frozen version never mutates). Only window **mutations** re-project, as publish units (§4.2).
**The projected row set (normative, D-121, 2026-07-31 review fix):** a plan-subject delta
projects **every price row of the plan — any lifecycle state except never-published drafts —
whose window set intersects `[projection_time − H, ∞)`**, each with its full interval set as
above, where **`H` = 2 × the longest billing cycle sold on the plan** (floor: one cycle + the
D-47 bulk batching SLO; tenant-configurable upward). The `expired` state and the horizon are
load-bearing: rating pins *current* versions and rates *past* instants (arrears always lag), so
after a changeover the superseded predecessor — its window now `expired` — must survive the
next re-projection or resolution at yesterday's `t` under the new delta fails closed on a
legitimately covered period. Anything older than `H` resolves by **replaying from the
originally-pinned version** (deltas are retained on the truth horizon, §3.7, so old pins stay
resolvable; posted periods never re-query per `fr-pricing-snapshot`). The bound is also the
size model: a delta is O(live keys × windows within `H`), never O(the plan's accumulated
history).

**Before Slice 7 the horizon is not evaluable, and the projection proceeds without it under a
stated premise (normative, D-167, 2026-08-03, found while building the read side).** The rule
above filters on a **window set**, and the `PriceWindow` store is Slice 7's: with no interval
columns the predicate cannot be evaluated at all, and taken literally the rule projects
**nothing** — no row has a window set, so no row intersects `[projection_time − H, ∞)`. What
the bound buys is the sentence above it, and a plan's accumulated history is its **superseded**
price rows; `published → superseded` has exactly two sanctioned producers, the **D-88**
supersession unit and the **D-100** cutover, and this gear has a producer for neither. So until
one lands the set the horizon filters is empty, the plan-subject delta is the plan's
`published` rows capped by the §14 per-plan soft cap, and the bound costs nothing because there
is nothing for it to exclude. **The group that builds either producer deletes this premise and
owes the horizon** — and with it `H`'s own input, "the longest billing cycle sold on the plan",
which is W6's term and is likewise unavailable here. This is the D-155 clause (3) shape on the
read side, and it is written where that group will meet it rather than left to be rediscovered.

`pricingSnapshotRef` is the composite reference (`CatalogVersion` + resolved price ids +
evaluation-policy version) pinned on charges and `BillableItem`s — stamped at publish with the
**pending** version ref, finalized to the committed `CatalogVersion` on
`CatalogVersionPublished`, and immutable thereafter; posted invoice periods MUST NOT re-query
mutable catalog rows. The **normative composition SoR is Tariffs**; the catalog-side view is
the aligned entry and MUST NOT diverge from it. **All three catalog-side parts are stamped by
the commit (normative, D-162, 2026-08-03).** The pending version ref and the resolved price ids
are stamped as described. The **evaluation-policy version** — named here, in
`fr-pricing-snapshot` and in Rating's composition contract as a segment this gear writes at
publish, and until D-162 given no producer, format or owner by any document of either gear
(D-161) — is the **evaluation-policy generation**, defined immediately below. Publish stamps
the generation the gear currently declares. It stamps a real value or none: D-161's ban on a
placeholder stands unchanged and is what the generation is built to respect, since Rating's
`SnapshotComposer` fail-closes on a missing pre-stamp (correctly) and would be *satisfied* by a
fabricated one, immutably and on posted money.

#### The evaluation-policy generation (normative, D-162)

The generation names **which evaluation-policy field set** the frozen row content of a snapshot
is to be read under. Its format is `ep-<n>`, `n` a positive integer, monotone, and **opaque to
consumers except for equality** — two snapshots carrying the same generation were frozen under
the same field set, and two carrying different generations were not. It is a **declared
constant** of this gear, not a per-publish value: every publish of one generation stamps the
same string, and the string moves only when a decision moves the field set.

The **roster** is the set of fields that tell an evaluator how to derive the billable quantity
and select the rate. It is **mostly** `pricing_price` columns and is not only those: the block
below rosters `usage_counter_on_plan_change`, which is a `pricing_plan` column and a
Slice-6-owned plan-change-contract field, admitted by `ep-2` (D-113) because a plan change's
counter handling decides how a quantity is derived across the boundary. The code splits the
roster the same way — `partition_row_fields` (12 members) and `partition_plan_fields` (1) —
so the shape is deliberate rather than a leak, and the definition is stated this way because
the sentence that stood here said `pricing_price` fields flatly and the block immediately
contradicted it (corrected 2026-08-17). It deliberately excludes the row's **identity** (the
scope-key axes and the metered line it prices), its **money** (amounts, bands, package price)
and the **consumer-contract** fields Slice 6 owns that govern *how a charge is billed* rather
than how a quantity is derived (proration, anchoring, billing timing, tax, rounding) — those
move under their own contracts and are not what this segment qualifies; the `outside:` list
below accordingly carries `allowed_change_targets` and `comparability_rank`, which are
`PlanChangeContract` fields on the excluded side of exactly that line. The
roster is spelled in the `snake_case` of the §6 column lists, one spelling, because the set
names these fields in three registers elsewhere and a guard cannot match prose.

The **bump rule**, precisely: a change **requires** a bump when it adds a field to the roster,
removes one, or moves a field across the roster boundary in either direction. A change
**requires no bump** when it changes a rostered field's *requiredness*, its enum's value set,
its default, or its meaning; when it adds a `pricing_price` field outside the roster; or when it
changes anything about money, identity or the Slice-6 contract fields. **What the generation
therefore does not claim** is stated rather than left to be discovered: it tracks the field
*set*, not the meaning of a field. D-58 made `tier_aggregation_window` mandatory on `package`
rows and would not bump it; a decision that redefined `peak` would not bump it either. A reader
carrying a generation carries the guarantee that the *shape* of the evaluation input has not
moved, and no guarantee at all about semantics inside that shape — the risk a content digest
would have covered and this does not ([`../DECISIONS.md`](../DECISIONS.md) D-162).

The roster, the fields deliberately outside it, and the generation log are declared here, and
this block is normative — the gear's guard reads it:

```text
evaluation-policy-generation: ep-4

log:
  ep-1  D-162  + model_kind
  ep-1  D-162  + package_size
  ep-1  D-162  + billing_granularity
  ep-1  D-162  + tier_aggregation_window
  ep-1  D-162  + tier_qualification_window
  ep-1  D-162  + aggregation_function
  ep-1  D-162  + aggregation_granularity
  ep-1  D-162  + max_hold_granules
  ep-1  D-162  + included_allowance
  ep-2  D-113  + usage_counter_on_plan_change
  ep-3  D-53   + reservation_flavor
  ep-4  D-68   + min_qty_usage
  ep-4  D-68   + min_qty_usage_fallback

outside:
  charge_kind          identity - scope-key axis 7
  meter                identity - the metered line the row prices
  dimension_key        identity - the line's dimension discriminator
  amount_minor         money
  unit_rate            money - the per_unit rate, D-311
  bands                money
  package_price_minor  money - D-122's "legitimate price lever"
  reserved_rate        rate - the committed reserved rate, S10 inst-rv-attrs, D-311 class
  min_qty_purchase     permission - order-time floor enforced by Subscriptions
  discount_ref         reference - the external instrument Promotions evaluates
  quantity_source      quantity origin, not quantity derivation
  manual_quantity      quantity origin, not quantity derivation
  allowed_change_targets  permission - whether a self-service change may happen
  comparability_rank      proration sign, computed by Subscriptions
```

The log is **append-only and replayed**: the roster is what applying its `+` and `-` operations
in order produces, the generations run `ep-1`, `ep-2`, … without gaps, and the last one is the
declared generation. That is the bump rule as a mechanism rather than as a request — a field
cannot join the roster without a log line, a log line is a generation, and the last generation
is the constant. Rewriting an existing line to smuggle a field into a past generation is
available and is not an evasion of the guard: it is a falsification of what a numbered decision
admitted, in a normative document, which is a different act with a different reviewer.

`ep-1` opens the log with the nine fields as they stand and claims **no retroactive history**.
D-44, D-45 and D-122 each moved this set before it was written down, and reconstructing
generations for them would mint versions no snapshot was ever stamped with — the same
fabrication D-161 clause (1) refuses, one artifact over.

**One of the nine is rostered and not yet authorable and one is half-authorable, and the roster is
right to name them anyway** (**D-177**, 2026-08-03; narrowed 2026-08-15): `tier_qualification_window`
is refused on every authoring path until Slice 10's remaining rules land, and `included_allowance`
is refused only where its `rolloverPolicy` is `carry` — the `{N, none}` half became authorable when
the `inst-ac-gate` refusals and the compile landed together. So a reader who takes this roster as
the list of fields a version *can* carry today will be wrong about one and a half of them. They stay because the log is
append-only and a `-` line would bump a generation for a field nobody can author — the fabrication
this section already refuses — and because the roster's subject is which field set a frozen row is
**read under**, which is a property of the generation and not of any surface's willingness to
accept a value. The refusal is what holds the line; see
[`10-advanced-primitives.md`](./10-advanced-primitives.md) §3 for what removing it requires.

On read-model outage the read path **fails
closed** (never serves stale). After a degraded publish the warm **re-drive continues past the
SLO**; on completion it sets the warm-completion marker (the version becomes resolvable —
monotonicity unaffected) and the degraded state ends, raising an operations alarm meanwhile;
no new event name is introduced — consumers observe completion through the **pin frontier** the
marker advances (§3.3/D-136; the marker itself is on no read surface, and this sentence named
it before the frontier existed — 2026-08-03 cleanup, no D-number).

**What "degraded" is, and what measures it (normative, D-166, 2026-08-03, found while building
the read side).** `fr-publish-fanout-atomicity` puts `PlanPublishDegraded` **after** the commit
— the version has committed and the warm has not landed within the 5s SLO — and puts the
pre-commit batching wait outside it, budgeted by D-47 and alarmed by
`pricing.catalogversion.commit_overdue` (§3.6). That separation needs an instant this gear did
not record: `requested_at` starts the *batching* clock, and `committed_at` is stamped by the
finalize, which never runs on the path where the warm is failing. Measured from `requested_at`
the 5s rule marks **every** publish degraded, including every one behaving exactly as D-47
budgets. So the ref row records **`commit_observed_at`** (§3.7) — when this gear first saw the
registry answer for that ref, written independently of whether the projection then lands — and
the degraded condition is `pending ∧ commit_observed_at set ∧ older than the 5s SLO`. It is an
upper bound: the observation lags the registry's own commit by at most one sweep pass, so the
signal is late rather than false, and the accurate form is the registry supplying its commit
instant with the version (the owed cross-gear half of D-163's adoption).

**The degraded state has no column, and needs none.** No table in §3.7 carries one:
`pricing_read_model` has no such field and `pricing_operator_flag` is forbidden from carrying
version state (D-85). It is the derived predicate above; completion clears it by making the
predicate false, which is exactly what "consumers observe completion" already means here. The
projector writes a subject's delta **warm in the same transaction that finalizes its ref**, so
"committed but unwarm" is unreachable in storage and the degraded state cannot be read off the
ref's committed-ness at all — which is the whole reason the observation instant has to be
recorded rather than derived.

**`pricing.readmodel.pin_eligibility_overdue` fires on the frontier's own `advanced_at` and
only in conjunction (normative, D-166).** A stale frontier alone is a tenant that has not
published — a tenant with no frontier row has never advanced and is stale by construction — so
staleness must be conjoined with something of this tenant actually short of pin-eligibility:
either a ref of its own past the max batching-delay SLO, or a committed version standing above
its frontier. The second arm is not redundant: a version **every** subject of which fails to
project is never committed in storage at all, the finalize and the warm sharing a transaction,
so on the exact path this section calls "a stuck version now holds the frontier" the
committed-version arm sees nothing. Finding a stale frontier needs a cross-tenant read, because
a per-tenant read cannot find a tenant whose frontier is stale precisely because nothing of it
has moved.

**A projection writes no audit row** (2026-08-03 cleanup, no D-number): `pricing_audit_log` is
the actor/before-after trail of a **mutation** (`inst-au-complete`, D-158), and a projection has
no actor and changes no truth — it materializes a decision already audited at its commit. The
silence was in the section a reader checks first.

The projection **source** is the **revision the version's own publish judged, and the lifecycle
state that judgement produced** — both frozen on the ref row at commit (`subject_revision`,
`subject_lifecycle_state`, §3.7) and read from there (**normative, D-165**, 2026-08-03, found
while building the read side; **amends D-128**, whose "the plan's current revision" was the
projector's source before). Reading the plan's *current* revision at projection time freezes
whatever is current when the sweep arrives — up to D-47's five-minute batching **maximum**
later, not the 5s warm SLO — into an INSERT-only delta on the ≥ 7-year horizon, in a store
whose whole contract is that a completed version never changes: revision 3 publishing into `V5`
and revision 4 into `V6` before `V5` warms gives `V5` revision 4's content, permanently. The
lifecycle state is the same fact one column over and moves faster: a pinned revision can stand
`published` when its own publish judged it and `superseded` by the time the sweep reads it, and
sellability predicate (4) then reads a version whose plan really was sellable as unsellable. A
`plan` subject arriving without either value is **refused**, never defaulted — a default is a
guess about which content a frozen version froze. D-128's substance is untouched and is what
makes reading a pinned revision safe at all: `(plan_id, revision)` is the same row whether it
stands `published` or `retired`, and retirement is a **publish unit of its own** that pins its
own revision with `retired` and re-projects it under its **own** version, so the state arrives
carrying its own version instead of leaking backwards into an older one. Only the two tokens
D-128 sanctions are storable, so a `superseded` state is not expressible in a version at all.
The plan's own truth rows for that revision, its revision-scoped child rows (D-83), and the
published price rows are the content — for the
initial warm and every re-drive alike; the projector **never reads the open draft revision**,
so a degraded-warm re-drive cannot leak draft edits into a frozen version (2026-07-30 review
fix). The revision's `lifecycle_state` is itself a **projected plan-subject field** — that is
what sellability predicate (4) reads at the pin (D-128). Versions store **per-subject deltas** (D-86, subject-typed by D-91): resolving `(pin, subject)`
reads the subject's greatest completed version ≤ the pin — monotonicity is unaffected, completed
versions never mutate, and retention follows the truth-history horizon (§3.7); overlay and
membership publish units resolve through their own subject rows, never through a plan's. The
versioned read model carries **no operator-plane state** (D-85): drift/divergence flags live in
`pricing_operator_flag` and never appear in a frozen version.

**The delta's payload is a declared vocabulary, and it declares what it cannot carry
(normative, D-167, 2026-08-03, found while building the read side).** No document named the
plan-subject delta's field list: this section says what it must *carry* (the window facts, the
lifecycle field, the D-121 row set), §3.7 says how it is *keyed*, and §3.1's `ReadModel` line is
a sketch — so the first implementation had to invent a schema, which is the D-158 shape one
store over. The wire keys are **`camelCase`, one spelling, declared once**: this section owns
the envelope — the subject's identity, the pinned revision and lifecycle state (D-165), and the
D-121 row set — and each slice declares the facts it owns into it, as the Slice-6 contract pair
is declared in [`06-consumer-contracts.md`](./06-consumer-contracts.md) §6. **A fact whose
owning slice has not landed is absent, and the absence is named rather than discovered:** before
Slices 4, 6, 7 and 10 and the registry `sellable` flag, a version carries neither `PriceWindow`
intervals, states and the derived coverage end (Slice 7 — D-99/D-121), nor the GA-gate flags
(Slice 4) and the prepaid-execution gate (Slice 10), nor the registry flag (D-46), nor the grant
set and the materialized phase→grant map (Slice 6 / D-41). `inst-sg-pinned`'s six predicates are
therefore a claim about the **finished** gear: a version produced today answers (2), (3) and (4)
and cannot answer (1), (5) or (6), and the slice that lands each fact is what makes its
predicate evaluable ([`07-pricewindow-linkage.md`](./07-pricewindow-linkage.md)
`inst-sg-pinned`). Operator flags are absent **by rule** rather than by omission (D-85, above),
and the distinction is stated here so a later reader completing the payload does not complete it
with them.

One reference is deliberately **not** version-pinned: a **`migrated-origin`** snapshot (Slice 11,
D-87) is **self-contained** — synthesis materializes the complete evaluable row content into the
frozen payload, and consumers evaluate from that payload without resolving its ids through the
read model (a resolved row's historical instant predates any useful pin, and D-76's second,
reference tier — which existed in no `CatalogVersion` at all — is struck by D-330, 2026-08-16;
the rule rests on the absent version either way). Because it resolves through **no** version, it
needs its own read surface, which the read-model contract does not provide: consumers fetch it
from `GET /bss-pricing/v1/migrated-origin-snapshots/{subscriptionRef}` (Slice 11 §5, `plan × read`
service identity — **D-102**, 2026-07-31 review fix; registered as an inbound lane of the Tariffs
contract, [`../PRD.md`](../PRD.md) §9.2). Everything else resolves version-pinned as above.

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) — §2.2 (canonical scope key), §6.2/§6.7 (model kind, publish, events), §6.8 (versioning/immutability/supersession), §6.9 (consumer resolution), §9 (interfaces), §17.4 (validation rules), §17.5 (change mechanisms + `CatalogVersion` increment)
- **DESIGN**: [`../DESIGN.md`](../DESIGN.md) — canonical index (slice map, dependency order, cross-cutting statements)
- **ADRs**: [`../ADR/`](../ADR/) — `cpt-cf-bss-pricing-adr-canonical-scope-key`, `cpt-cf-bss-pricing-adr-grandfathering-cohort-axis`, `cpt-cf-bss-pricing-adr-pricewindow-consolidation`

**Traces to**: `cpt-cf-bss-pricing-fr-publish-validation-failclosed`,
`cpt-cf-bss-pricing-fr-published-rows-append-only`, `cpt-cf-bss-pricing-fr-plan-versioning`,
`cpt-cf-bss-pricing-fr-supersession`, `cpt-cf-bss-pricing-fr-pricing-snapshot`,
`cpt-cf-bss-pricing-fr-consumer-readmodel-resolution`,
`cpt-cf-bss-pricing-fr-catalogversion-increment`,
`cpt-cf-bss-pricing-fr-publish-fanout-atomicity`, `cpt-cf-bss-pricing-fr-event-contract`,
`cpt-cf-bss-pricing-fr-price-amount-validation`, `cpt-cf-bss-pricing-fr-mutation-idempotency`,
`cpt-cf-bss-pricing-fr-concurrent-edit`

Grouped by theme:

- the aggregate fail-closed validation pipeline (`fr-publish-validation-failclosed`)
- append-only history + versioning/supersession (`fr-published-rows-append-only` / `fr-plan-versioning` / `fr-supersession`)
- snapshot + monotonic read model (`fr-pricing-snapshot` / `fr-consumer-readmodel-resolution`)
- `CatalogVersion` request, degraded handling, frozen event set (`fr-catalogversion-increment` / `fr-publish-fanout-atomicity` / `fr-event-contract`)
- money/precision, idempotency, optimistic concurrency (`fr-price-amount-validation` / `fr-mutation-idempotency` / `fr-concurrent-edit`)
- the two primary API contracts (`cpt-cf-bss-pricing-interface-authoring-publish` / `cpt-cf-bss-pricing-interface-catalog-read-model`)
