---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-07-20 by Virtuozzo International GmbH

# Mandatory idempotency key on every ingestion record

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Mandatory idempotency key, plugin-enforced deduplication](#mandatory-idempotency-key-plugin-enforced-deduplication)
  - [Optional idempotency key with best-effort dedup](#optional-idempotency-key-with-best-effort-dedup)
  - [Server-generated idempotency key](#server-generated-idempotency-key)
- [More Information](#more-information)
- [Traceability](#traceability)
- [Amendment 2026-07-17 — `created_at` moves to the dedup identity (see ADR-0014)](#amendment-2026-07-17--created_at-moves-to-the-dedup-identity-see-adr-0014)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-mandatory-idempotency`

## Context and Problem Statement

At-least-once delivery is the operational baseline across REST and SDK callers, so retries are routine and duplicate submissions are expected. Counters and gauges have different correctness profiles — duplicate counter records inflate accumulated totals; duplicate gauge records poison rate-of-change and distinct-timestamp signals — but both require deduplication for correctness. The question is whether the ingestion contract requires an idempotency key on every record, encourages one optionally, or relies on backend-side deduplication after the fact. The decision shapes the ingestion-contract obligations, the rejection error surface, the active plugin's storage schema, and the calling-gear ergonomics for retry on transient failure. A same-key collision is not uniformly safe to absorb: an exact-equality retry that re-sends identical content is the benign case, but a key reused with different content is a caller bug that must be surfaced rather than silently dropped.

## Decision Drivers

- `cpt-cf-usage-collector-fr-idempotency` — idempotency key is required on the ingestion contract; the core delegates dedup-on-conflict to the active plugin via the Plugin SPI.
- `cpt-cf-usage-collector-fr-counter-semantics` — counter-semantics enforcement requires that retries do not inflate accumulated totals.
- `cpt-cf-usage-collector-nfr-ingestion-latency` — idempotency enforcement is on the synchronous hot path and must fit within the 200 ms p95 budget.
- PRD §5.1 at-least-once semantics (`cpt-cf-usage-collector-fr-idempotency` rationale) — retry safety must be uniform across counter and gauge kinds without per-kind retry strategies in calling gears.
- `cpt-cf-usage-collector-principle-fail-closed` — keyless records must be rejected with a deterministic error rather than silently accepted.

## Considered Options

- Mandatory idempotency key on every record, plugin-enforced deduplication — the ingestion contract requires the key; the core rejects keyless records; the active plugin enforces uniqueness via a `UNIQUE` constraint on the composite `(tenant_id, gts_id, idempotency_key)` and returns dedup-on-conflict.
- Optional idempotency key with best-effort dedup — callers may supply a key; if present, the plugin deduplicates; if absent, records are stored as-is.
- Server-generated idempotency key — the core derives a fingerprint from the record's content (e.g., hash of attribution + timestamp + value); callers do not supply a key.

## Decision Outcome

Chosen option: "Mandatory idempotency key on every record, plugin-enforced deduplication", because it is the only option that makes at-least-once delivery safe uniformly across counter and gauge kinds without forcing calling gears to implement kind-dependent retry strategies. The ingestion contract on SDK, REST, and Plugin SPI all require the key; the core rejects keyless records with a deterministic error; the active plugin enforces a `UNIQUE` constraint on the composite `(tenant_id, gts_id, idempotency_key)` per `plugin-spi.md` §"Cross-entity invariants honored by the Plugin SPI". A same-key submission resolves into one of two distinct outcomes. When all caller-supplied canonical fields (value, timestamp, resource_ref, subject_ref, and metadata) equal the stored record, it is an exact-equality retry: the plugin returns a deduplicated outcome and the core surfaces a successful but deduplicated acknowledgement. When any of those canonical fields differs — including a metadata-only difference — it is a canonical-field mismatch: the plugin returns `PersistOutcome::Conflict` (carrying the existing record id), and the core rejects it fail-closed via a new `idempotency_conflict` reason mapped to the AlreadyExists/409 category. The second write is never silently dropped. The idempotency window is unbounded: the key never expires, has no time-to-live (TTL), and is never intentionally reusable, so the `UNIQUE (tenant_id, gts_id, idempotency_key)` constraint is permanent; storage plugins MUST preserve that key tuple permanently even when record bodies are purged or archived by retention, so retention MUST NOT free a dedup key.

### Consequences

- The ingestion contract grows a mandatory `idempotency_key` field across SDK trait, REST API, and Plugin SPI; callers cannot omit it.
- Source gears adopt a retry pattern that distinguishes payloads: same key + identical payload = safe retry (deduplicated); same key + different payload = `Conflict`; this removes per-kind retry logic from emitters while keeping accidental key reuse visible.
- Callers that reuse a key with different content receive a deterministic `idempotency_conflict` rejection (AlreadyExists/409), not a silent drop, so a key-reuse bug cannot mask divergent data from billing and downstream consumers.
- The idempotency window is unbounded (no TTL, never reusable), and storage plugins MUST preserve the `(tenant_id, gts_id, idempotency_key)` tuple permanently through retention — purge or archive of record bodies MUST NOT free a dedup key.
- The active plugin owns the dedup primitive (a `UNIQUE` constraint on the composite `(tenant_id, gts_id, idempotency_key)` per `plugin-spi.md` §"Cross-entity invariants honored by the Plugin SPI" and a conflict-handling path); the core does not maintain a dedup table.
- The ingestion path takes a deterministic rejection error for keyless requests, surfaced through the same error contract as other validation failures.
- Idempotency keys are caller-chosen; the platform documentation specifies the recommended shape (e.g., ULID, UUIDv7) but the contract does not enforce it.

### Confirmation

Compliance is confirmed through (a) ingestion contract tests rejecting keyless records on every surface, (b) duplicate-submission tests for counter and gauge kinds covering both arms — an exact-equality retry yielding a deduplicated acknowledgement, and a same-key submission with at least one differing canonical field yielding an `idempotency_conflict` rejection — (c) Plugin SPI conformance tests asserting the active plugin enforces the `UNIQUE` constraint on the composite `(tenant_id, gts_id, idempotency_key)`, and (d) a Plugin SPI conformance test that reuse of a key whose record body has been purged or archived by retention still rejects (the dedup key remains preserved).

## Pros and Cons of the Options

### Mandatory idempotency key, plugin-enforced deduplication

Every record carries a caller-supplied key; the plugin enforces uniqueness via a database-level constraint on the composite `(tenant_id, gts_id, idempotency_key)` per `plugin-spi.md` §"Cross-entity invariants honored by the Plugin SPI".

- Good, because retry safety is uniform across counter and gauge kinds without kind-dependent emitter logic.
- Good, because the dedup primitive lives at the storage layer where it is cheapest and most correct.
- Good, because surfacing a `Conflict` on key reuse with different content prevents silently masking a caller bug, protecting billing and downstream consumers from divergent data hidden behind a reused key.
- Good, because keyless requests fail closed deterministically, matching the fail-closed principle.
- Neutral, because callers must generate the key; the platform documents a recommended shape but does not enforce a specific format.
- Bad, because mandatory contract fields are a breaking-change risk if relaxed later; the contract-stability ADR governs this.

### Optional idempotency key with best-effort dedup

Callers may supply a key; if present, the plugin deduplicates; otherwise records are stored as-is.

- Good, because the contract is permissive and reduces friction for casual callers.
- Bad, because best-effort dedup makes counter-correctness depend on caller discipline, which is the gap the FR exists to close.
- Bad, because emitter-side retry semantics become kind-dependent (gauges might retry, counters might not), which is the failure mode the FR explicitly avoids.
- Bad, because operational dashboards and billing pipelines downstream lose the strong guarantee that all records are dedup-protected.

### Server-generated idempotency key

The core derives a key from a hash of attribution + timestamp + value; callers do not supply one.

- Good, because callers do not need to generate or persist keys.
- Bad, because the derivation cannot distinguish a legitimate duplicate (intended replay) from a coincidental repeat (two distinct emissions that happen to share attribution + timestamp + value), particularly for gauges with low cardinality.
- Bad, because retries from the same emitter may produce different timestamps and therefore different server-generated keys, defeating the dedup purpose.
- Bad, because the derivation logic lives in the core and becomes a maintenance burden tied to the dedup primitive.

## More Information

Related decisions: `cpt-cf-usage-collector-adr-pluggable-storage` (the SPI through which the dedup primitive is enforced); `cpt-cf-usage-collector-adr-caller-supplied-attribution` (the attribution fields that participate in the dedup boundary). The `plugin-spi.md` §"Cross-entity invariants honored by the Plugin SPI" `UNIQUE (tenant_id, gts_id, idempotency_key)` contract and the §3.2 ingestion path are the structural anchors.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-idempotency` — mandatory idempotency key on the ingestion contract.
- `cpt-cf-usage-collector-fr-counter-semantics` — retry safety for counters; no inflated totals on replay.
- `cpt-cf-usage-collector-nfr-ingestion-latency` — keeps idempotency enforcement on the synchronous hot path within the 200 ms p95 budget.
- `cpt-cf-usage-collector-principle-idempotency-by-key` — codifies the principle in §2.1.
- `IdempotencyKey` and `cpt-cf-usage-collector-interface-plugin` — the entity and SPI surface participating in the `UNIQUE (tenant_id, gts_id, idempotency_key)` composite that enforces deduplication.

## Amendment 2026-07-17 — `created_at` moves to the dedup identity (see ADR-0014)

[`ADR-0014`](./0014-created-at-in-dedup-identity.md) makes `created_at` part of
the dedup identity: the dedup key becomes the 4-tuple
`(tenant_id, gts_id, idempotency_key, created_at)` and `created_at` is removed
from the canonical-field comparison set. Consequently a same-key submission with
a *different* `created_at` is two distinct records rather than an
`IdempotencyConflict` — narrowing, for the timestamp field only, the "same key +
different canonical field ⇒ Conflict" rule in the Decision above. Same key +
same `created_at` + any other canonical field differing (`value`,
`resource_ref`, `subject_ref`, `corrects_id`, `metadata`) remains an
`IdempotencyConflict`. The permanent-dedup-key-preservation obligation is
unchanged. Rationale and full consequences live in ADR-0014; this Decision body
is otherwise unchanged.
