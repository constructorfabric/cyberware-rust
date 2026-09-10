---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-06-10 by Virtuozzo International GmbH

# Monotonic deactivation as the cross-kind error retraction primitive

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Monotonic one-way deactivation](#monotonic-one-way-deactivation)
  - [Reversible deactivation](#reversible-deactivation)
  - [Hard delete](#hard-delete)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-monotonic-deactivation`

## Context and Problem Statement

Individual usage records occasionally need to be retracted from the active dataset — for example, when an emitting gear discovers a misclassification, or when an operator must withdraw a record that was accepted in error. The question is how the lifecycle of an individual record should be exposed: as a one-way status transition, as a reversible status field with reactivation, as a hard delete, or as a mutable record with a soft-delete column. The decision affects the query semantics observable by downstream consumers, the storage plugin's history representation, and the operator's mental model of how a record can change after acceptance.

This ADR is scoped to **cross-kind error retraction**: the operator-driven act of declaring "this record should not have been accepted; remove it from active query results." Retraction is the only correction available for `gauge` records and for the `COUNT`, `MIN`, `MAX`, and `AVG` aggregations on any kind, because those aggregations cannot be netted by appending a signed entry. The complementary primitive — _value-reversal_ for `counter` records (refunds, partial releases, credit-style corrections that net inside `SUM`) — is owned by a separate ADR (`cpt-cf-usage-collector-adr-usage-compensation`, file `./0008-usage-compensation.md`) and is explicitly out of scope here. This ADR therefore does **not** claim to cover refunds, partial releases, or any form of signed value adjustment.

## Decision Drivers

- `cpt-cf-usage-collector-fr-event-deactivation` — deactivation is a status-only operator-driven transition exposed via SDK and REST; downstream consumers rely on stable, well-defined record semantics across time.
- `cpt-cf-usage-collector-principle-monotonic-deactivation` — codifies the one-way property.
- PRD §1.3 operator-self-service goal — the operator gains a deterministic lifecycle event to reason about without the collector taking on mutable-record semantics.
- `cpt-cf-usage-collector-constraint-no-business-logic` — the collector does not own pricing or billing logic; retraction is a recording-level action, not a computed adjustment.

## Considered Options

- Monotonic one-way deactivation — `status` transitions from `active` to `inactive` exclusively; no reactivation, no other field mutation; applies uniformly to any `UsageRecord` — both ordinary usage rows (`corrects_id IS NULL`) and compensation rows (`corrects_id IS NOT NULL`).
- Reversible deactivation — status field with both deactivate and reactivate operations.
- Hard delete — deactivation removes the record from the active dataset; recovery is operational rather than API-driven.

## Decision Outcome

Chosen option: "Monotonic one-way deactivation", because it gives storage plugins, query consumers, and aggregation pipelines a first-class lifecycle event to reason about without re-introducing mutable-record semantics into the metering substrate. The Usage Collector exposes a SDK and REST operation that transitions a record's `status` field from `active` to `inactive`; the core authorizes the operator via Policy Decision Point (PDP), validates the current status, and dispatches the one-way status update to the plugin without altering any other record field. Reactivation is not available; deactivation requests against already-inactive records are rejected deterministically.

The retraction primitive applies uniformly to **any** `UsageRecord` regardless of `corrects_id` presence — both ordinary usage rows (those with `corrects_id IS NULL`) and compensation rows (those with `corrects_id IS NOT NULL`) can be deactivated through the same operation. Deactivating an ordinary usage row (a `UsageRecord` with `corrects_id IS NULL`) that has one or more `active` compensation rows whose `corrects_id` equals the deactivated row's id triggers a **depth-1 cascade**: those referencing compensation rows are flipped to `inactive` in the same set-flip step, so the net `SUM` returns to the state it held before either the ordinary usage row or its compensations were accepted. The cascade is strictly depth-1: compensations are not themselves compensable by construction, because the L1 referential check defined in `cpt-cf-usage-collector-adr-usage-compensation` rejects any `corrects_id` that targets a row whose own `corrects_id IS NOT NULL` (variant `corrects_id_targets_compensation`); no further cascade levels exist. Value-reversal cases that net inside `SUM` (refunds, partial releases) are deferred entirely to `cpt-cf-usage-collector-adr-usage-compensation`; this ADR does not cover them.

### Consequences

- The `status` field has exactly two values (`active`, `inactive`) and one allowed transition (`active → inactive`); the plugin enforces the monotonicity at the storage layer for every `UsageRecord` regardless of `corrects_id` presence.
- Deactivating an ordinary usage row (a `UsageRecord` with `corrects_id IS NULL`) cascades depth-1: any `active` compensation rows whose `corrects_id` equals the deactivated row's id are flipped to `inactive` in the same set-flip step. No deeper cascade exists because compensations are non-compensable by construction (the L1 `corrects_id_targets_compensation` check rejects any `corrects_id` that targets a row whose own `corrects_id IS NOT NULL`).
- Downstream consumers can include or exclude inactive records via query filters; aggregation pipelines treat `inactive` as a first-class signal rather than a soft-delete leak. `SUM`, `COUNT`, `MIN`, `MAX`, and `AVG` over `active` rows automatically reflect the cascade because deactivated compensations stop contributing to the net total.
- No record field other than `status` is ever modified after acceptance; the metering substrate stays append-only-with-status-flag instead of becoming mutable. The latch is one-way `active → inactive` and operator-only.
- Operator workflows for retiring records are unambiguous; mistaken deactivations are corrected by issuing a fresh ingestion (with a new idempotency key) and PDP-authorized attribution, not by reactivation.
- Plugin authors must enforce the monotonicity at the storage layer for every `UsageRecord` regardless of `corrects_id` presence (both `corrects_id IS NULL` and `corrects_id IS NOT NULL` rows) and execute the depth-1 cascade atomically with the parent flip; the contract test for the Plugin SPI covers both behaviours.

### Confirmation

Compliance is confirmed through (a) sequence-diagram review of the deactivation flow, (b) a Plugin SPI Method 5 depth-1 set-flip contract test asserting that deactivating an ordinary usage row (a `UsageRecord` with `corrects_id IS NULL`) flips every `active` compensation row whose `corrects_id` equals the target id in the same atomic step and that no further depth is traversed, (c) a contract test asserting `active → inactive` is the only allowed transition and that the latch applies uniformly to every `UsageRecord` regardless of `corrects_id` presence, (d) a cascade query test asserting that after the parent flip, no `active` compensation rows remain pointing at the deactivated parent, and (e) authorization tests confirming PDP gates the operation on the operator's `SecurityContext` plus the record's attribution.

## Pros and Cons of the Options

### Monotonic one-way deactivation

`active → inactive` is the only allowed status transition; reactivation and other field mutations are not exposed; the primitive applies uniformly to every `UsageRecord` (regardless of `corrects_id` presence) and cascades depth-1 from a deactivated ordinary usage row (`corrects_id IS NULL`) to its referencing `active` compensation rows (those with `corrects_id` equal to the deactivated row's id).

- Good, because it gives downstream consumers a stable, first-class lifecycle event without making the record mutable.
- Good, because it keeps the storage layer effectively append-only with a status flag, which is straightforward to enforce in every plausible backend.
- Good, because the operator mental model is simple: deactivation is final, and the depth-1 cascade keeps net totals consistent without operator follow-up.
- Good, because applying uniformly across both row kinds means a single operation covers both retraction of a wrongly-emitted ordinary usage row and retraction of a wrongly-emitted compensation row.
- Neutral, because operators occasionally need to recover from mistaken deactivations; the workflow is to issue a fresh record, which is consistent with the idempotency-key model.
- Neutral, because the chosen primitive does not perform value-reversal — partial corrections and refund-style adjustments are handled by the complementary compensation primitive in `cpt-cf-usage-collector-adr-usage-compensation`, which nets inside `SUM` without retracting the original record.
- Bad, because there is no in-API recovery for mistaken deactivations; operator tooling must make the consequence clear before the action commits.

### Reversible deactivation

Status field with both deactivate and reactivate operations.

- Good, because operators can recover from mistaken deactivations without issuing a fresh record.
- Bad, because it re-introduces mutable-record semantics; downstream consumers can no longer assume the status field is monotone across time.
- Bad, because aggregation pipelines need to handle `inactive → active` transitions and the resulting historical re-inclusion of records in retrospective queries.
- Bad, because the storage primitive becomes a mutable status column rather than an append-only-with-flag, expanding the contract surface and the test matrix for plugins.

### Hard delete

Deactivation physically removes the record from the active dataset; recovery is operational (backup restore) rather than API-driven.

- Good, because the active dataset stays free of inactive records, simplifying queries that do not need them.
- Bad, because audit trails, billing reconciliation, and historical aggregations lose the ability to include or exclude deactivated records cleanly.
- Bad, because hard deletes do not compose with the mandatory idempotency contract: a fresh record with the same key after deletion is ambiguous (replay or replacement?).
- Bad, because the lifecycle event is not first-class for downstream consumers, who must infer deletion from absence.

## More Information

Related decisions:

- `cpt-cf-usage-collector-adr-usage-compensation` (file `./0008-usage-compensation.md`) — the complementary primitive for counter value-reversal (refunds, partial releases); this ADR delegates all signed-`SUM`-netting corrections there.
- `cpt-cf-usage-collector-adr-mandatory-idempotency` (file `./0004-mandatory-idempotency.md`) — the contract that lets a corrective fresh-record workflow work cleanly when an operator deactivates in error.
- `cpt-cf-usage-collector-adr-pluggable-storage` (file `./0002-pluggable-storage.md`) — the Plugin SPI seam where monotonicity and the depth-1 cascade are enforced.

**2026-05-29 narrowing-of-scope note**: this ADR was narrowed (without status change) to remove the over-claim that deactivation covered refund / partial-release / value-reversal cases and to drop the earlier "monotonically increasing total" framing. The original accepted decision — that deactivation is a one-way `active → inactive` latch — is preserved verbatim; only the _scope_ of what the latch is responsible for was tightened. Reason: the team split correction into two primitives, retaining retraction here and moving value-reversal to the new `cpt-cf-usage-collector-adr-usage-compensation`.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **Related ADRs**: [`./0008-usage-compensation.md`](./0008-usage-compensation.md) (`cpt-cf-usage-collector-adr-usage-compensation`), [`./0004-mandatory-idempotency.md`](./0004-mandatory-idempotency.md) (`cpt-cf-usage-collector-adr-mandatory-idempotency`), [`./0002-pluggable-storage.md`](./0002-pluggable-storage.md) (`cpt-cf-usage-collector-adr-pluggable-storage`).

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-event-deactivation` — status-only one-way transition exposed via SDK and REST; applies uniformly to every `UsageRecord` regardless of `corrects_id` presence; preserves stable, append-only-with-flag semantics for downstream consumers.
- `cpt-cf-usage-collector-principle-monotonic-deactivation` — codifies the principle in §2.1.
- `UsageRecord` and `cpt-cf-usage-collector-seq-deactivate-event` — the entity and sequence realizing the transition and its depth-1 cascade.
- `cpt-cf-usage-collector-component-deactivation-handler` — the §3.2 component that owns the operation.
