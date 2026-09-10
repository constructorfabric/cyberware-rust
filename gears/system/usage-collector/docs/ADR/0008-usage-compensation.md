---
status: accepted
date: 2026-05-29
decision-makers: usage-collector spec owners
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-06-10 by Virtuozzo International GmbH

# Usage compensation as a signed negative entry on the unified ingestion path

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Signed compensation entry on the unified ingestion path](#signed-compensation-entry-on-the-unified-ingestion-path)
  - [Adjustment-by-reference / record amendment](#adjustment-by-reference--record-amendment)
  - [Downstream-only correction](#downstream-only-correction)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-usage-compensation`

## Context and Problem Statement

Counter-kind usage records accumulate via `SUM` aggregation, and operational realities — refunds, partial releases, credit-style adjustments — require the net total to decrease without retracting the original event. The retraction primitive in `cpt-cf-usage-collector-adr-monotonic-deactivation` (file `./0005-monotonic-deactivation.md`) handles whole-row error retraction, but it cannot express a partial reduction: it flips the entire row to `inactive`, removing all of its value from the net total. A second correction primitive is therefore needed for the value-reversal case, and the question is how to expose it. The decision must preserve three invariants already accepted across the Usage Collector spec: the append-only invariant on stored records (no in-place mutation), the backend-agnostic posture (no concrete storage-engine assumptions), and the recording-not-computing posture (the collector records caller-supplied quantities; it does not derive business logic). It must also remain compatible with the mandatory idempotency contract (`cpt-cf-usage-collector-adr-mandatory-idempotency`, file `./0004-mandatory-idempotency.md`) and with Policy Decision Point (PDP) authorization that resolves the calling gear's identity from the caller's `SecurityContext`.

## Decision Drivers

- Append-only invariant: stored records must not be mutated in place.
- Backend-agnostic posture: the primitive must not assume any particular storage engine or transactional model beyond what the Plugin SPI already requires.
- PDP attribution at the source: corrections must carry the same caller-supplied attribution as the original emission, gated by the same PDP boundary.
- Mandatory idempotency: every correction must carry an idempotency key on the unified ingestion path.
- `SUM`-nets aggregation: the net `SUM(value)` over `active` rows must equal the corrected net total without per-record reconciliation logic in the collector.
- Counter-only scope: gauges, `COUNT`, `MIN`, `MAX`, and `AVG` cannot be netted by appending a signed entry — those cases are owned by retraction (`cpt-cf-usage-collector-adr-monotonic-deactivation`).
- No L2 remaining-amount tracking: the collector does not track per-record outstanding balances or lot/FIFO-LIFO state — that is downstream-ledger work.
- Concurrency safety vs. deactivation: a compensation that arrives while its target row is being deactivated must not race past the deactivation; the ingestion-time L1 validation must reject it cleanly.
- Single structural discriminator: the row kind (ordinary usage versus counter compensation) is carried by a single field — the `corrects_id` pointer — and is not duplicated by a separate stored type tag.

## Considered Options

- Signed compensation entry on the unified ingestion path — the calling gear emits a `UsageRecord` whose `corrects_id` field is set to the identifier of the original ordinary usage row and whose `value` is negative, through the existing emit endpoint / SDK method / SPI persist call. Same PDP attribution, same mandatory idempotency key, same validation lane (extended by `(UsageKind × corrects_id presence)` matrix), same storage row shape — presence of `corrects_id` is itself the structural marker that distinguishes a compensation row from an ordinary usage row.
- Adjustment-by-reference / record amendment — mutate the original ordinary usage row in place (for example, add a `corrected_value` column or rewrite `value`).
- Downstream-only correction — the Usage Collector never records corrections; consumers reconcile via their own ledgers.

## Decision Outcome

Chosen option: "Signed compensation entry on the unified ingestion path", because it preserves the append-only invariant, reuses the existing emit / PDP / idempotency machinery without adding a parallel ingestion surface, and yields `SUM`-based netting deterministically with no business-logic computation inside the collector. The calling gear emits a new `UsageRecord` whose `corrects_id` field carries the identifier of the original ordinary usage row and whose `value` is negative; the row travels through the existing emit operation, REST endpoint, SDK method, and Plugin SPI persist call, with the same caller-supplied attribution and the same mandatory idempotency key. No dedicated `compensate` REST path, SDK method, or SPI call is introduced.

The presence of `corrects_id` is the sole structural discriminator between the two row kinds: an **ordinary usage row** is a `UsageRecord` whose `corrects_id` is unset (`None` in the SDK, `IS NULL` in storage); a **compensation row** is a `UsageRecord` whose `corrects_id` is set (`Some(<record-id>)` in the SDK, `IS NOT NULL` in storage) and targets the active ordinary usage row whose id it carries. There is no separate stored type tag.

The Usage Collector spec now defines two complementary correction primitives. **Deactivation** (`cpt-cf-usage-collector-adr-monotonic-deactivation`) is cross-kind error retraction: a whole-row, one-way `active → inactive` latch, operator-only, applying to any `UsageRecord` regardless of `corrects_id` presence, with a depth-1 cascade from a deactivated ordinary usage row to its referencing `active` compensations. **Compensation** (this ADR) is counter value-reversal: an append-only negative-value `UsageRecord` carrying `corrects_id` that reduces `SUM`, counter-only, calling-gear-emitted on the ingestion path with PDP authorization (whose calling-gear identity is read from the caller's `SecurityContext`) and a mandatory idempotency key. The two primitives are disjoint by purpose (retraction versus value-reversal) and complementary by aggregation contract: deactivation removes a row from every aggregation; compensation reduces the netted `SUM` only.

The validation matrix governing what is accepted on ingestion is:

| UsageKind  | `corrects_id` | Outcome                                  |
| ---------- | ------------- | ---------------------------------------- |
| `counter`  | IS NULL       | `value >= 0`                             |
| `counter`  | SET           | `value < 0`                              |
| `gauge`    | IS NULL       | any value                                |
| `gauge`    | SET           | REJECTED → `gauge_compensation_rejected` |

The aggregation contract is: `SUM(value)` over `active` rows nets across both row kinds — ordinary usage rows (`corrects_id IS NULL`) and compensation rows (`corrects_id IS NOT NULL`) — so `SUM(value)` is the net total. `COUNT`, `MIN`, `MAX`, and `AVG` operate over `active` rows `WHERE corrects_id IS NULL` only — compensation rows adjust `SUM`; they are not events.

The L1 referential check on `corrects_id` performed at ingestion is: the referenced row MUST exist (else `corrects_id_not_found`), MUST share the full identity tuple `(tenant_id, gts_id, resource_ref, subject_ref)` with the incoming compensation — `subject_ref` presence is part of the identity, so `None` vs `Some(_)` is a scope mismatch — (else `corrects_id_wrong_scope`), MUST be `active` (else `corrects_id_inactive`), and MUST itself be an ordinary usage row — that is, MUST have `corrects_id IS NULL` (else `corrects_id_targets_compensation`). There is no L2 layer: the collector does not track per-record remaining amounts, lot/FIFO-LIFO state, or whether multiple compensations together exceed the original value. Concurrency between compensation and deactivation is resolved at the L1 check: a compensation referencing a row that is currently being deactivated is rejected by the `corrects_id_inactive` clause; the deactivation cascade then flips any already-accepted compensations depth-1, leaving the net total consistent. Compensating a compensation is structurally impossible — `corrects_id_targets_compensation` enforces it at L1 — which is why the deactivation cascade is bounded at depth 1 by construction.

### Consequences

- The ingestion contract (SDK, REST, Plugin SPI persist) carries an optional `corrects_id` field on the `UsageRecord`. Its presence (set / `IS NOT NULL`) marks the row as a compensation; its absence (unset / `IS NULL`) marks the row as an ordinary usage row. The `value` field is signed; the previously-implicit non-negative invariant moves into the `(UsageKind × corrects_id presence)` matrix.
- The validation matrix is applied at the ingestion boundary: `counter` + `corrects_id SET` requires `value < 0`; `gauge` + `corrects_id SET` is rejected with `gauge_compensation_rejected`; `counter` + `corrects_id IS NULL` keeps `value >= 0`; `gauge` + `corrects_id IS NULL` accepts any value.
- `SUM(value)` over `active` rows is the net total — callers and storage plugins must understand that `SUM` nets signed entries across both row kinds. `COUNT`, `MIN`, `MAX`, and `AVG` continue to operate over `active` rows `WHERE corrects_id IS NULL` only; the query layer filters on `corrects_id` presence before applying those aggregations.
- A compensation referencing a row that is currently being deactivated is rejected by the L1 `corrects_id_inactive` check; this provides concurrency safety without requiring distributed coordination.
- The depth-1 cascade in `cpt-cf-usage-collector-adr-monotonic-deactivation` is sufficient because the L1 `corrects_id_targets_compensation` check makes compensating a compensation structurally impossible — no second-order chains exist.
- The Usage Collector does not validate non-negative net totals and does not emit negative-net detection signals; "net went negative" is a downstream concern, not a collector responsibility.
- The Usage Collector does not compute refunds, credits, credit-notes, quota, or lot/FIFO-LIFO depletion; recording a caller-supplied negative quantity is recording, not computing. Per-record remaining-amount tracking (an "L2" lane) is explicitly out of scope.
- Callers must compute the negative `value` themselves; reviewers must remember that `SUM(value)` is the net total; tooling that reads raw rows must understand that `corrects_id` presence selects between ordinary usage rows and compensation rows.

### Confirmation

Compliance is confirmed through (a) a Plugin SPI contract test that persists a `UsageRecord` carrying a set `corrects_id` and a negative `value`, then asserts `SUM(value)` over `active` rows nets correctly across ordinary usage rows and compensation rows, (b) a validation matrix contract test covering the four `(UsageKind × corrects_id presence)` cells — `counter` + `corrects_id IS NULL` accepts `value >= 0`, `counter` + `corrects_id SET` accepts `value < 0`, `gauge` + `corrects_id IS NULL` accepts any value, `gauge` + `corrects_id SET` is rejected with `gauge_compensation_rejected` — (c) a concurrency contract test asserting that a compensation referencing a row that is being deactivated is rejected by the L1 `corrects_id_inactive` check, (d) a depth-1 cascade test (also covered by `cpt-cf-usage-collector-adr-monotonic-deactivation`) confirming that deactivating an ordinary usage row flips its `active` compensation rows to `inactive` in the same atomic step, and (e) `corrects_id` L1 referential tests asserting the four error variants — `corrects_id_not_found`, `corrects_id_wrong_scope`, `corrects_id_inactive`, and `corrects_id_targets_compensation` — fire under their respective preconditions.

## Pros and Cons of the Options

### Signed compensation entry on the unified ingestion path

The calling gear emits a `UsageRecord` whose `corrects_id` field is set to the identifier of the original ordinary usage row and whose `value` is negative, through the same emit endpoint / SDK method / SPI persist call already in use for ordinary usage rows.

- Good, because it preserves the append-only invariant — the original ordinary usage row is never mutated; the compensation is a new row that nets in `SUM`.
- Good, because it reuses the existing PDP attribution, mandatory idempotency, and Plugin SPI contract without introducing a parallel ingestion surface — the contract carries a single optional `corrects_id` field, and presence/absence is itself the discriminator.
- Good, because `SUM`-based netting is deterministic, backend-agnostic, and requires no business-logic computation inside the collector.
- Good, because the depth-1 deactivation cascade composes naturally — deactivating the target ordinary usage row also flips its compensation rows, keeping the net total consistent under retraction.
- Neutral, because callers must compute the negative `value` themselves; the collector does not derive it from a refund-percentage or release-quantity parameter.
- Neutral, because reviewers of stored data must remember that `SUM(value)` is the net total — raw-row inspection without aggregation can be misleading.
- Bad, because tooling that reads raw rows must learn the `corrects_id` column to interpret aggregation correctly; non-aware consumers can misinterpret a negative `value`.
- Bad, because the `value` field is signed, which subtly broadens the contract — a typed-language SDK must surface this clearly to avoid accidental positive compensations.

### Adjustment-by-reference / record amendment

Mutate the original ordinary usage row in place (for example, add a `corrected_value` column or rewrite `value`).

- Good, because the net total can be read from a single column without understanding signed entries.
- Bad, because it breaks the append-only invariant that the rest of the storage substrate relies on.
- Bad, because it conflicts with deactivation's whole-row latch — what is the state of an "amended-and-then-deactivated" row?
- Bad, because it complicates idempotency: an amendment with the same key as the original is ambiguous (replay or amendment?), and an amendment with a fresh key creates a non-atomic two-row history that downstream consumers cannot reason about.
- Bad, because computing a partial reduction (refund, partial release) inside the collector pushes it into business-logic territory — the collector becomes a mini-ledger, contradicting the recording-not-computing posture.
- Bad, because plugin authors must implement in-place mutation atomically across read and write paths, which is harder to enforce uniformly across plausible backends.

### Downstream-only correction

The Usage Collector never records corrections; consumers reconcile via their own ledgers.

- Good, because the collector contract stays minimal: only ordinary usage rows ever exist.
- Bad, because the value-reversal primitive lives outside the source of record, leaving a permanent gap in the audit trail.
- Bad, because every downstream consumer must build its own reconciliation layer to interpret `SUM`, multiplying integration cost and creating divergence between consumers.
- Bad, because PRD-level capabilities that depend on `SUM`-based net totals (billing reads, dashboard sums) become unreliable without consumer-side reconciliation, breaking the contract the PRD already commits to.
- Bad, because operators lose the ability to express refunds and partial releases at the source — a regression versus what the deactivation primitive already provides for whole-row retraction.

## More Information

Related decisions:

- `cpt-cf-usage-collector-adr-monotonic-deactivation` (file `./0005-monotonic-deactivation.md`) — the complementary cross-kind error retraction primitive; deactivation cascades depth-1 to `active` compensation rows referencing a deactivated ordinary usage row, keeping the net total consistent under retraction. The two ADRs jointly define the Usage Collector's correction model.
- `cpt-cf-usage-collector-adr-mandatory-idempotency` (file `./0004-mandatory-idempotency.md`) — compensation rides the same unified ingestion path and therefore carries a mandatory idempotency key with the same exact-equality-versus-conflict semantics. Same-key replay of a compensation is deduplicated; same-key reuse with different content is rejected as `idempotency_conflict`.
- `cpt-cf-usage-collector-adr-pluggable-storage` (file `./0002-pluggable-storage.md`) — the Plugin SPI seam through which the signed `value` and the optional `corrects_id` column are persisted and `SUM`-nets aggregation is realized.

Non-goals explicitly out of scope for this ADR:

- Compensating a compensation (the L1 `corrects_id_targets_compensation` check makes it structurally impossible, and the depth-1 cascade in `cpt-cf-usage-collector-adr-monotonic-deactivation` is sufficient by construction because of this exclusion).
- Positive or otherwise-signed compensations beyond the locked `value < 0` rule for `counter` + `corrects_id SET`.
- L2 enforcement of per-record remaining amounts, outstanding balances, or any lot / FIFO-LIFO tracking.
- Negative-net detection, alerting, or rejection inside the Usage Collector.
- Computing refunds, credits, credit-notes, or quota inside the Usage Collector.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **Related ADRs**: [`./0005-monotonic-deactivation.md`](./0005-monotonic-deactivation.md) (`cpt-cf-usage-collector-adr-monotonic-deactivation`), [`./0004-mandatory-idempotency.md`](./0004-mandatory-idempotency.md) (`cpt-cf-usage-collector-adr-mandatory-idempotency`), [`./0002-pluggable-storage.md`](./0002-pluggable-storage.md) (`cpt-cf-usage-collector-adr-pluggable-storage`).

This decision directly addresses or constrains the following requirements and design elements (IDs marked **forward** are minted by Phases 2–4 of the active plan and become canonical when those phases land):

- `cpt-cf-usage-collector-fr-usage-compensation` — the FR carrying the counter value-reversal capability surface (**forward**, minted in Phase 3).
- `cpt-cf-usage-collector-fr-counter-semantics` — counter accumulation semantics; `SUM` nets signed entries across ordinary usage rows and compensation rows.
- `cpt-cf-usage-collector-fr-gauge-semantics` — gauge semantics; `gauge` + `corrects_id SET` is rejected by the validation matrix with `gauge_compensation_rejected`.
- `cpt-cf-usage-collector-fr-idempotency` — mandatory idempotency key on the unified ingestion path; compensation rides the same contract.
- `cpt-cf-usage-collector-fr-ingestion` — the unified ingestion capability carries the optional `corrects_id` field.
- `cpt-cf-usage-collector-fr-ingestion-authorization` — PDP attribution applies to compensation identically to ordinary usage.
- `cpt-cf-usage-collector-fr-query-aggregation` — `SUM` nets signed across both row kinds; `COUNT`/`MIN`/`MAX`/`AVG` operate over `active` rows `WHERE corrects_id IS NULL`.
- `cpt-cf-usage-collector-fr-event-deactivation` — depth-1 cascade interaction with the retraction primitive.
- `UsageRecord` — the record entity carries the optional `corrects_id` field.
- `cpt-cf-usage-collector-interface-plugin` — the SPI persist surface carries the nullable `corrects_id` attribution; `SUM(value)` nets across records of both kinds.
- `cpt-cf-usage-collector-seq-emit-usage` — the emission sequence carries the optional `corrects_id` field on the unified ingestion path.
- `cpt-cf-usage-collector-usecase-emit` — the emit use case covers both ordinary usage ingestion (`corrects_id` unset) and compensation ingestion (`corrects_id` set).
