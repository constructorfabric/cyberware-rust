---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-09-10 by Virtuozzo International GmbH

# Mandatory idempotency key on every ingestion entry

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Same-key outcomes](#same-key-outcomes)
  - [Idempotency horizon](#idempotency-horizon)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Mandatory idempotency key, plugin-enforced deduplication](#mandatory-idempotency-key-plugin-enforced-deduplication)
  - [Optional key with best-effort deduplication](#optional-key-with-best-effort-deduplication)
  - [Server-generated key](#server-generated-key)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-mandatory-idempotency`

## Context and Problem Statement

At-least-once delivery is the operational baseline for REST and SDK callers.
Retries are routine, so duplicate submissions are expected.

Each GTS type declares one aggregation fold from a closed set, and a duplicate
harms every fold. Under `SUM` a retry inflates the accrued total, with no way to
detect or correct it. Under any other fold a duplicate observation poisons
consumers that derive counts, distinct observation windows, or rate-of-change
signals from raw entries. Deduplication is therefore required for correctness
whatever the fold.

The question is whether the ingestion contract requires an idempotency key on
every entry, merely encourages one, or relies on backend deduplication after the
fact. The answer shapes the contract obligations, the rejection error surface,
the plugin's storage schema, and how a calling gear retries a transient failure.

A same-key collision is not uniformly safe to absorb. An exact-equality retry
that re-sends identical content is the benign case. A key reused with different
content is a caller bug, and the gear must surface it rather than drop it.

## Decision Drivers

- `cpt-cf-usage-collector-fr-idempotency` — the ingestion contract requires an
  idempotency key, and the gear delegates deduplication to the active plugin.
  Retry safety must hold for every declared fold, so a calling gear needs one
  retry pattern rather than a fold-dependent one.
- `cpt-cf-usage-collector-fr-aggregation-fold` — a retry under a `SUM` fold
  inflates the accrued total.
- `cpt-cf-usage-collector-nfr-ingestion-latency` — enforcement sits on the
  synchronous ingestion path and must fit the 200 ms p95 budget.
- `cpt-cf-usage-collector-principle-fail-closed` — a keyless entry must draw a
  deterministic rejection, never silent acceptance.

## Considered Options

- Mandatory idempotency key, plugin-enforced deduplication — the contract
  requires the key, and the gear rejects a keyless entry. The active plugin
  enforces uniqueness of the dedup identity at the storage layer.
- Optional key with best-effort deduplication — a caller can supply a key. The
  plugin deduplicates when one is present, and stores the entry as-is when it is
  absent.
- Server-generated key — the gear derives a fingerprint from the entry's own
  content, and a caller supplies no key.

## Decision Outcome

Chosen option: "Mandatory idempotency key, plugin-enforced deduplication". It is
the only option that makes at-least-once delivery safe for every declared fold
without a fold-dependent retry strategy in every calling gear.

The REST, SDK, and Plugin SPI contracts all require the key, and the gear rejects
a keyless entry with a deterministic error. Both discriminants carry a mandatory
key, because a measurement and an invalidation ride the same ingestion path.

The dedup identity is the tenant, the GTS type, the idempotency key, and both
bounds of the covered period. The active plugin enforces its uniqueness at the
storage layer, and the gear keeps no dedup table of its own. Two submissions
under one key that cover different periods are therefore distinct entries, not a
conflict.

That identity is deliberately minimal, so the key carries what it leaves out.
Resource and subject are compared on a collision but are not part of the
identity, which puts one obligation on a caller: a key that varies with
everything the identity omits, repeated exactly on a retry.

### Same-key outcomes

A same-key submission inside the horizon resolves into one of two outcomes.

It is an exact-equality retry when all six caller-supplied canonical fields match
the stored entry: quantity, resource, subject, metadata, invalidation target, and
reason code. The plugin absorbs the submission and returns the stored entry, and
the gear acknowledges it as accepted. No surface carries a separate duplicate
outcome. A caller that must tell a replay from a first write reads the returned
acceptance instant and acceptance sequence, which are the first entry's.

It is a canonical-field mismatch when any of those fields differs, a
metadata-only difference included. The plugin reports a conflict that carries the
existing entry's identifier, and the gear rejects the submission fail-closed in
the already-exists error category
(`cpt-cf-usage-collector-principle-canonical-errors`). The second write is never
silently dropped.

Entry type is not compared. The gear derives it from the invalidation target, so
it cannot differ unless the target differs. The target is compared, and it enters
neither the dedup identity nor the identifier derivation. That is what makes one
key reused across a measurement and its withdrawal a conflict rather than two
accepted entries.

### Idempotency horizon

Retention bounds the horizon. It is not unbounded. A dedup identity stays visible
to later submissions for at least as long as the referenced GTS type's retention
policy keeps that entry. The horizon runs from the covered period, not from
acceptance, so it is per-meter. A storage plugin must hold the identity unique
over at least that span.

The horizon is a floor rather than an exact boundary. A deployment never has to
retain a dedup identity beyond the data it protects, and a purge or archive of an
entry frees its dedup identity with it. A purge runs on the plugin's own
schedule, so an aged entry can sit in the store after its horizon ends, and its
dedup identity stays live for as long as it does.

No admissible submission reaches the far side of the floor. The covered period
enters the dedup identity, and it is also what the ingestion bounds are checked
against — the same two timestamps serve both purposes. No path admits a period
ending further back than the backfill window
(`cpt-cf-usage-collector-adr-backfill-isolation`), and every meter's retention is
at least that window plus one replay horizon
(`cpt-cf-usage-collector-fr-billing-retention-floor`). The window therefore sits
strictly inside retention on every meter rather than merely within it, and a
submission old enough for its counterpart to have been purged is refused on the
bound, before deduplication is consulted at all. The horizon is a floor the
ingestion surface cannot walk off.

No caller therefore carries a post-horizon obligation, because on the ingestion
side there is no past the horizon. A consumer still deduplicates on the entry
identifier (`cpt-cf-usage-collector-fr-record-identity`), for the at-least-once
feed delivery that is its own reason.

### Consequences

- The ingestion contract carries a mandatory idempotency key on every surface. A
  caller cannot omit it.
- A calling gear adopts one retry pattern. The same key with identical content is
  a safe retry, and the same key with different content is a conflict.
  Fold-dependent retry logic disappears from every emitter.
- A caller that reuses a key with different content receives a deterministic
  conflict rejection rather than a silent drop. A key-reuse bug therefore cannot
  hide divergent data from billing and other consumers.
- The active plugin owns the deduplication primitive and the conflict path. The
  gear maintains no dedup table.
- A keyless request draws a deterministic rejection through the same error
  contract as any other validation failure.
- A caller chooses its own keys. The contract bounds the key's length and forbids
  control characters, so that the identifier derivation stays injective. No token
  shape is enforced. A caller that generates an opaque key, such as a ULID or a
  UUIDv7, must therefore store it before the first send.
- An imported entry already carries a partly spent horizon, because retention
  runs from the covered period rather than from import time. What remains is
  never exhausted while the entry is still submittable: an import reaching past
  the backfill window is refused on the bound, so a re-run either lands inside
  the horizon and is deduplicated, or does not land. There is no third case in
  which the gear admits a silent duplicate.

### Confirmation

- Ingestion contract tests that reject a keyless entry on every surface.
- Duplicate-submission tests over both arms and every declared fold. An
  exact-equality retry yields an accepted acknowledgement carrying the stored
  entry, and a same-key submission with one differing canonical field yields a
  conflict rejection.
- A test that two resources measured in one period under one key yield one
  accepted entry and one conflict.
- Plugin SPI conformance tests that assert the active plugin holds the dedup
  identity unique.
- A test that walks a submission's covered period from inside the backfill window
  to beyond it, asserting the far side yields a bound rejection rather than any
  deduplication outcome.
- A plugin SPI conformance test for the horizon, driving the plugin directly
  rather than the ingestion route, which refuses an over-aged period on the
  bound before deduplication is consulted. A replay inside the horizon resolves
  to the stored entry or a conflict. Beyond it the test asserts only that the
  submission draws one of three outcomes, selected by whether the plugin has
  purged the counterpart: acceptance as a new entry once it has, the stored
  entry while it has not and every canonical field matches, or a conflict while
  it has not and one differs. The gear guarantees no single one, because the
  purge schedule is the plugin's.

## Pros and Cons of the Options

### Mandatory idempotency key, plugin-enforced deduplication

Every entry carries a caller-supplied key. The plugin enforces the dedup
identity's uniqueness with a storage-level constraint.

- Good, because the ingestion path never consults the declared fold. The contract
  carries no fold-dependent special case, so an emitter uses one retry pattern
  for every GTS type it emits.
- Good, because the deduplication primitive lives at the storage layer, where it
  is cheapest and most correct.
- Good, because a conflict on key reuse with different content keeps a caller bug
  visible. Billing and other consumers stay protected from divergent data behind
  a reused key.
- Good, because a keyless request fails closed deterministically, which matches
  the fail-closed principle.
- Neutral, because a caller must generate the key and must repeat it on a retry.
  The contract bounds the key's length and charset and enforces no token shape.
- Bad, because a mandatory contract field is a breaking-change risk if it is
  relaxed later. `cpt-cf-usage-collector-adr-contract-stability` governs that
  risk.

### Optional key with best-effort deduplication

A caller can supply a key. The plugin deduplicates when one is present, and
otherwise stores the entry as-is.

- Good, because the contract is permissive and costs a casual caller less effort.
- Bad, because duplicate safety then depends on caller discipline instead of a
  guarantee, which is the gap that `cpt-cf-usage-collector-fr-idempotency` exists
  to close.
- Bad, because without a guaranteed key, emitter retry logic becomes
  fold-dependent again.
- Bad, because dashboards and billing pipelines downstream lose the guarantee
  that every entry is dedup-protected.

### Server-generated key

The gear derives a key from the entry's attribution, timestamp, and quantity. A
caller supplies none.

- Good, because a caller needs to generate and persist no key.
- Bad, because the derivation cannot separate a legitimate replay from a
  coincidental repeat that shares attribution, timestamp, and quantity. A
  low-cardinality quantity makes this likely.
- Bad, because retries from one emitter can carry different timestamps, and
  therefore different derived keys, which defeats the purpose.
- Bad, because the derivation logic then lives in the gear and becomes a
  maintenance burden tied to the deduplication primitive.

## More Information

Related decisions:

- `cpt-cf-usage-collector-adr-pluggable-storage` — the SPI that enforces the
  deduplication primitive.
- `cpt-cf-usage-collector-adr-caller-supplied-attribution` — the attribution
  fields that enter the dedup boundary.
- `cpt-cf-usage-collector-adr-record-identity-derivation` — the entry identifier
  that a consumer deduplicates on across at-least-once feed delivery.
- `cpt-cf-usage-collector-adr-backfill-isolation` — the window bound that keeps
  every admissible submission inside the horizon.
- `cpt-cf-usage-collector-adr-append-only-invalidation` — the invalidation entry
  whose reason code joins the canonical-field set.
- `cpt-cf-usage-collector-adr-contract-stability` — governs any later relaxation
  of the mandatory field.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-idempotency` — the mandatory idempotency key on the
  ingestion contract.
- `cpt-cf-usage-collector-fr-aggregation-fold` — a retry under a `SUM` fold
  inflates the accrued total.
- `cpt-cf-usage-collector-fr-record-quantity` — the quantity that a retry
  duplicates.
- `cpt-cf-usage-collector-nfr-ingestion-latency` — keeps enforcement on the
  synchronous ingestion path inside the 200 ms p95 budget.
- `cpt-cf-usage-collector-principle-idempotency-by-key` — the design principle
  that this decision codifies.
- `cpt-cf-usage-collector-principle-canonical-errors` — the error contract that
  carries the conflict rejection.
- `cpt-cf-usage-collector-interface-plugin` — the SPI surface that enforces the
  dedup identity.
- `cpt-cf-usage-collector-fr-record-identity` — the entry identifier that a
  consumer deduplicates on across at-least-once feed delivery.
- `cpt-cf-usage-collector-fr-usage-windows` — the covered period whose bounds
  enter the dedup identity.
- `cpt-cf-usage-collector-fr-billing-retention-floor` — the per-meter retention
  that sets the horizon.
