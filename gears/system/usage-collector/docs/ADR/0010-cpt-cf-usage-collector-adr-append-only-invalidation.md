---
status: accepted
date: 2026-08-17
decision-makers: usage-collector spec owners
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

# Invalidation as the single correction primitive on an append-only ledger

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Five rules at the gateway, one at the store](#five-rules-at-the-gateway-one-at-the-store)
  - [Fold exclusion over both entries of the pair](#fold-exclusion-over-both-entries-of-the-pair)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Faithful-copy invalidation entry on the ordinary ingestion path](#faithful-copy-invalidation-entry-on-the-ordinary-ingestion-path)
  - [Signed compensation entry netting inside `SUM`](#signed-compensation-entry-netting-inside-sum)
  - [One-way status flip with a depth-1 cascade](#one-way-status-flip-with-a-depth-1-cascade)
  - [Sparse invalidation marker carrying only a reference](#sparse-invalidation-marker-carrying-only-a-reference)
  - [In-place update of the accepted entry](#in-place-update-of-the-accepted-entry)
  - [Downstream-only correction](#downstream-only-correction)
- [More Information](#more-information)
  - [Why withdrawal is the primitive that generalises](#why-withdrawal-is-the-primitive-that-generalises)
  - [Three accepted losses](#three-accepted-losses)
  - [Open question — bulk withdrawal](#open-question--bulk-withdrawal)
  - [Related decisions](#related-decisions)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-append-only-invalidation`

## Context and Problem Statement

An emitter sometimes gets a measurement wrong. A mis-measurement, a
mis-attribution, and an emitter defect all produce the same need: something the
gear has already accepted must stop counting. The gear is the source of record
for rated usage, so it must offer one answer to that need.

Two properties of the gear constrain every candidate answer. A GTS type declares
one fold from `SUM`, `COUNT`, `MAX`, `MIN`, and `LATEST`, and the gear resolves it
at read time. A correction must therefore work under all five, and it can read no
write-side meter kind. A paginated feed scan also observes a snapshot
(`cpt-cf-usage-collector-adr-consistency-contract`), so the gear can change no
entry it has already delivered.

The question is what single correction primitive holds under every declared fold
and leaves every accepted entry untouched.
`cpt-cf-usage-collector-fr-record-invalidation` states the resulting model
normatively, and this document records the decision behind it.

## Decision Drivers

- `cpt-cf-usage-collector-principle-append-only-ledger` — an accepted entry is
  never rewritten, retired, or altered, and no operation on any surface mutates
  one.
- `cpt-cf-usage-collector-fr-record-invalidation` — states the correction model
  normatively and leaves the reasoning here.
- The feed snapshot guarantee in
  `cpt-cf-usage-collector-adr-consistency-contract` — a paginated scan observes a
  snapshot, so the gear changes no delivered entry.
- One rule across every fold — a GTS type declares one fold from five, and a
  correction must work under all of them.
- No write-side meter classification — the ingestion path carries no meter kind,
  so no primitive can depend on one.
- Surface economy — a correction reuses the ingestion path, the PDP boundary, the
  idempotency contract, and the ingestion quotas that already exist.
- Reconstructable audit trail — both the measurement and its withdrawal stay
  readable after the correction lands.

## Considered Options

- Faithful-copy invalidation entry on the ordinary ingestion path — an appended
  entry that copies its target field for field. It marks itself, keys itself,
  references the target, and gives a reason.
- Signed compensation entry netting inside `SUM` — the emitter appends an entry
  carrying a correction reference and a negative quantity, and `SUM` nets the
  pair.
- One-way status flip with a depth-1 cascade — an operator flips a stored status
  flag from active to inactive. The flip cascades once, to the compensations that
  point at the flipped entry.
- Sparse invalidation marker carrying only a reference — an appended entry that
  names the target and copies nothing from it.
- In-place update of the accepted entry — the gear rewrites the original
  quantity, or adds a second quantity column beside it, and appends nothing.
- Downstream-only correction — the gear records no correction at all, and each
  consumer reconciles in its own ledger.

## Decision Outcome

Chosen option: "Faithful-copy invalidation entry on the ordinary ingestion path".

The gear is an append-only ledger, and no operation on any surface changes an
entry it has already accepted. A correction is expressed by appending an
**invalidation entry** that refers to exactly one accepted entry. It travels the
same ingestion path that carries a measurement, so no dedicated correction
endpoint, SDK method, or storage-plugin call exists. The platform PDP authorizes
it on the caller's identity exactly as it authorizes a measurement, and it
carries the mandatory idempotency key
(`cpt-cf-usage-collector-adr-mandatory-idempotency`).

The entry is a **faithful copy** of its target. Every caller-supplied field equals
the target's: tenant, GTS type, resource, subject, covered period, quantity, and
metadata. The departures from that copy are closed, and are exactly three. The
entry carries its own idempotency key distinct from the target's, the reference
to the target, and a reason code. The reference is what makes the entry an
invalidation, so no separate marker exists to disagree with it.

The entry also gets four server-side fields in its own right rather than by copy
(`cpt-cf-usage-collector-fr-record-invalidation`). They are the fields any
accepted entry gets: an identifier, an acceptance instant, an acceptance-sequence
position, and an origin marker. The gear derives the identifier and stamps the
instant and the marker, and the storage plugin assigns the sequence. The marker
names the path the entry arrived on, and the sequence gives the invalidation its
own acceptance position on the feed.

### Five rules at the gateway, one at the store

The Ingestion Gateway enforces five of the six invalidation rules before it
dispatches the entry to the store.

- **Explicit reference.** The entry names the record it withdraws, and that
  reference is what identifies it as an invalidation. It is never recognised by
  the value or the sign of its quantity. The reference and the reason code are
  both-or-neither.
- **Valid reference.** The reference resolves to an existing entry. A reference
  that resolves to nothing is rejected with an actionable error.
- **No invalidation of an invalidation.** The target is itself a measurement. An
  invalidation entry never refers to another invalidation entry.
- **Faithful copy.** Every copied field matches the target's. The presence of a
  subject against its absence is itself a mismatch, and a rejection names the
  field that differs.
- **Reason code.** The entry carries a reason code
  (`cpt-cf-usage-collector-fr-invalidation-reason-code`).

The covered period is **not** among these rules. The entry copies its target's
period, and that period is validated against the bounds of the path the entry
arrived on, exactly as a measurement's own period is
(`cpt-cf-usage-collector-adr-backfill-isolation`). Withdrawal therefore reaches
back exactly as far as emission does on the same route, and no further.

The sixth rule belongs to the store rather than to the gateway.

- **At most one per entry.** At most one invalidation exists per entry, and a
  second one is rejected. An exact-equality resubmission under the same
  idempotency key is absorbed as a duplicate rather than treated as a second
  invalidation.

Only the store can make that check atomic with the entry it admits, in a single
backend transaction. A gateway-side pre-read cannot exclude a concurrent second
submission, so at-most-one is the plugin's one invalidation obligation. Reference
validity is plugin-observable too, because a point lookup that misses raises a
not-found error. The gateway is therefore not its sole enforcer.

### Fold exclusion over both entries of the pair

A fold excludes both entries of the pair, not the target alone. The withdrawn
measurement contributes nothing, and the invalidation entry contributes nothing.
The rule is load-bearing for correctness rather than tidiness, because a fold that
admits the echoed quantity double-counts the measurement it was meant to remove.

Both entries carry the same covered period, so no requested covered-period range
selects one of the pair without the other. Aggregation and raw query both select
by that range, so no placement of the invalidation changes a result.

The usage feed is the contrary case. It orders by acceptance sequence, and an
invalidation lands at its own acceptance position. That position can fall on a
later page than its target, so a feed page can carry a target without its
invalidation. A feed consumer tolerates that ordering, and never treats a target
as final merely because the page carrying it held no invalidation.

Every ledger read path — raw query, point lookup, and usage feed — returns both
entries as persisted, the invalidation naming its target.

### Consequences

- The model carries no status field, no lifecycle flag, and no active or inactive
  state anywhere. Withdrawal is a property of an appended fact rather than of
  stored state.
- The fold exclusion above is load-bearing for correctness. The echoed quantity
  lets a reader of the withdrawal alone see what is withdrawn, and a read path
  that admits it double-counts.
- Any materialised aggregate recomputes over the affected range rather than
  absorbs an appended correction. `MAX`, `MIN`, and `LATEST` reverse under no
  additional term at all. Storage engines also differ in whether they can observe
  the obligation, so each plugin publishes an invalidation-propagation bound
  (`cpt-cf-usage-collector-nfr-aggregate-freshness`).
- Invalidation is not an erasure path. Both entries stay persisted and readable
  exactly as accepted. An operator who must remove data uses retention.
- **A quantity correction is not one atomic act.** The withdrawal and the
  replacement are two separate submissions. A feed consumer can read the
  withdrawal before the replacement arrives, and never treats that as a settled
  net-zero period. The replacement carries the same attribution and the same
  covered period as the withdrawn entry. A submission that changes either is a
  different measurement rather than a correction of this one.
- **A partial reduction is expressible on no surface.** An emitter that needs one
  withdraws the whole entry and re-emits a corrected measurement. That is two
  entries and more traffic than one signed entry costs.
- **A correction cannot itself be reversed.** An accepted invalidation is
  permanent. The model forbids invalidating an invalidation and caps withdrawal at
  one per entry, so a wrong withdrawal has no in-gear recovery.
- One rule governs the entry, so nothing has to be restated per field as the entry
  shape grows.
- The ledger read paths carry no filter that selects withdrawn entries in or out.
  The target reference an invalidation carries is what a consumer reads instead.
  A consumer that folds entries it read itself leaves a withdrawn pair out on its
  own side.
- Withdrawal is not an operator-only action. Any caller the PDP authorizes for the
  target's attribution can withdraw an entry under it
  (`cpt-cf-usage-collector-fr-ingestion-authorization`), and the reason code
  carries the intent.
- **A withdrawal of a closed period travels the backfill route**, because the
  copied period decides the route. It therefore carries `origin = backfill`,
  which is what lets a consumer read it as a correction of history. A bulk
  withdrawal after an emitter defect runs under that route's workload isolation
  rather than competing with live ingestion. The route is on the SDK as well as
  on REST, so an emitter needs no operator escalation to use it
  (`cpt-cf-usage-collector-fr-backfill`).
- An emitter holds or recomputes the target's caller-supplied fields to build the
  copy. The derived identifier is reproducible offline
  (`cpt-cf-usage-collector-adr-record-identity-derivation`), so naming the target
  costs no read-back.

### Confirmation

- A contract test asserting that a faithful-copy mismatch is rejected with an
  error naming the field that differs, covering the subject
  presence-against-absence case.
- A store contract test asserting that the at-most-one-invalidation check commits
  atomically with the entry it admits, and rejects a concurrent second
  invalidation.
- A test asserting that an exact-equality resubmission under the same idempotency
  key is absorbed as a duplicate rather than treated as a second invalidation.
- A test asserting that an invalidation whose target predates the live past
  tolerance is rejected on the live path with an error naming the backfill
  route, and accepted on that route carrying `origin = backfill`.
- An aggregation test asserting that every declared fold excludes both entries of
  the pair.
- A plugin test asserting that a materialised aggregate recomputes after a
  withdrawal, rather than absorbing an appended term.

## Pros and Cons of the Options

### Faithful-copy invalidation entry on the ordinary ingestion path

An appended entry copies every caller-supplied field of its target, and departs
from that copy in exactly three closed ways.

- Good, because it leaves every accepted entry untouched, so the feed snapshot
  guarantee survives intact.
- Good, because one rule covers every declared fold, with no interpretation of
  what a quantity means.
- Good, because it reuses the ingestion path, the PDP boundary, the idempotency
  contract, and the ingestion quotas.
- Good, because the copied period makes the entry's own identifier derivable, and
  keeps both entries of the pair inside any range that selects either.
- Good, because the copied metadata keeps the withdrawal inside the same grouped
  and filtered reads that surfaced the target.
- Neutral, because the copy costs the emitter only the fields it already sent.
- Bad, because it cannot express a partial reduction at all, only a whole-entry
  withdrawal.
- Bad, because the fold exclusion becomes load-bearing for correctness rather than
  for tidiness.

### Signed compensation entry netting inside `SUM`

The emitter appends an entry carrying a correction reference and a negative
quantity, and `SUM` nets the pair.

- Good, because it preserves the append-only invariant. The original entry is
  never mutated, and the correction is a new entry.
- Good, because it reuses the existing PDP attribution, the mandatory idempotency
  key, and the Plugin SPI persist call. It adds one optional field rather than a
  parallel ingestion surface.
- Good, because `SUM` netting is deterministic and backend-agnostic. It puts no
  business logic inside the gear, which only records a caller-supplied quantity.
- Good, because a materialised `SUM` absorbs the appended negative term with no
  recomputation. The chosen primitive obliges a recomputation instead, which is a
  real cost this decision accepts.
- Good, because it expresses a **partial** reduction, which a whole-entry
  withdrawal cannot express at all. A refund or a partial release costs one entry
  rather than two.
- Neutral, because the caller computes the negative value itself. The gear derives
  nothing from a refund percentage or a release quantity, which keeps it out of
  ledger territory.
- Bad, because it nets only under `SUM`. Every other fold reverses under no
  additional term, so the model needs a companion primitive for every other case.
- Bad, because its sign rule reads a write-side meter classification. A matrix of
  meter kind against correction-reference presence needs the ingestion path to
  know a counter from a gauge. The gear carries no meter kind.
- Bad, because the sign of a stored value then carries structural meaning. A
  reader that does not join on the correction reference cannot separate a real
  decrease from a correction.

### One-way status flip with a depth-1 cascade

An operator flips a stored status flag one way, from active to inactive. The flip
cascades once, to the compensations that point at the flipped entry.

- Good, because it gives a consumer a first-class lifecycle event to reason about,
  instead of an absence to infer.
- Good, because it is uniform across meters. One operation retracts a whole entry
  under every fold, including the ones no signed term can reverse.
- Good, because every plausible backend enforces a one-way flag cheaply. A
  single-column update plus a monotonicity constraint is enough.
- Good, because it keeps the store append-only-with-a-flag rather than mutable. No
  field other than the flag ever changes after acceptance.
- Good, because the depth-1 cascade keeps net totals consistent with no operator
  follow-up. The depth bound holds by construction, because the gear rejects a
  compensation that targets a compensation.
- Neutral, because it offers no in-API recovery from a mistaken flip. The workflow
  is a fresh emission under a new idempotency key, which matches the idempotency
  model already in force.
- Bad, because it mutates an entry the gear has already delivered. A paginated feed
  scan can observe the flip, and the feed snapshot guarantee forbids exactly that.
- Bad, because it makes withdrawal a property of stored state. Every read path then
  carries a status filter, and a plugin that omits the filter leaks a withdrawn
  entry silently.

### Sparse invalidation marker carrying only a reference

The entry names its target, marks itself, and gives a reason. It copies no
attribution, no covered period, no quantity, and no metadata.

- Good, because the entry is small, and the emitter holds only the target's
  identifier.
- Good, because ingestion performs one referential check and no field-by-field
  comparison.
- Bad, because an entry without the target's covered period has no derivable
  identity. The identifier is computed over the period, so the entry cannot be
  identified at all.
- Bad, because an entry carrying a period of its own falls outside the range an
  auditor scans to re-read a closed month. That scan returns the withdrawn entry
  without the withdrawal.
- Bad, because an entry without the target's metadata drops out of exactly the
  grouped and filtered reads that surfaced the target. A consumer narrowing by a
  declared property sees the measurement and never its withdrawal.
- Bad, because a reader of the withdrawal alone cannot tell what is being
  withdrawn. Every such read costs a round-trip to the target.

### In-place update of the accepted entry

The gear mutates the entry it already accepted. It rewrites the quantity, or adds
a second quantity column beside the original. Nothing is appended, and no second
entry exists.

- Good, because a consumer reads the net total from one field. It needs no
  understanding of paired entries and no fold-side exclusion rule.
- Good, because it expresses a partial reduction directly, and it holds one row per
  measurement rather than two.
- Neutral, because it needs no reference, no marker, and no second idempotency key.
  The correction is addressed by the target's own identifier.
- Bad, because it destroys the audit trail. The measurement as accepted is no
  longer readable, so no query reconstructs what the gear reported before the
  correction.
- Bad, because it breaks the append-only principle outright rather than bending it.
  It mutates an entry the gear has already delivered, which the feed snapshot
  guarantee forbids, and every read path then serves changeable state.
- Bad, because a second quantity column doubles the quantity surface. Every fold,
  every plugin, and every consumer then has to decide which field it means.

### Downstream-only correction

The gear records no correction. Each consumer reconciles withdrawals in its own
ledger.

- Good, because the gear's contract stays minimal. Only measurements ever reach the
  ledger.
- Bad, because the withdrawal lives outside the source of record. The audit trail
  carries a permanent gap that no query against the gear can close.
- Bad, because every consumer must build its own reconciliation layer. Two
  consumers reading one ledger can then report two different totals.
- Bad, because a wrong measurement stays inside every aggregate the gear serves.
  The gear publishes figures it already knows to be untrue.

## More Information

### Why withdrawal is the primitive that generalises

The positive reason this primitive works, where a signed term and a status flip do
not, is narrow and load-bearing. Withdrawal is the only correction whose meaning
does not depend on what a quantity means. An entry that was never true contributes
nothing under `SUM`, `COUNT`, `MAX`, `MIN`, and `LATEST` alike. That is what lets
a single rule cover every meter.

A signed adjustment fails that test. It has to be interpreted differently for an
accrued amount than for an observation. Subtracting from an accrued total is
meaningful, and subtracting from an observed level is not. A status flip fails for
an unrelated reason: it mutates an entry that a consumer already holds.

### Three accepted losses

This decision accepts three named losses against a signed-term model. The partial
reduction a signed entry expresses is available on no surface. A quantity
correction is not atomic, because the withdrawal and the replacement are two
submissions. And a correction cannot be reversed, because the model forbids
invalidating an invalidation.

### Open question — bulk withdrawal

The gear withdraws one entry per invalidation entry, and the case this under-serves
is the one that matters most in practice: an emitter defect that produces a large
number of wrong entries. A predicate-shaped bulk operation interacts with ingestion
quotas, with the scope of the recomputation it forces, and with the feed contract.
A consumer cannot deduplicate a predicate by entry identifier, so the withdrawal
still has to reach the feed as one entry per entry withdrawn. The PRD records the
question, and this decision does not settle it.

### Related decisions

- `cpt-cf-usage-collector-adr-mandatory-idempotency` — states the dedup identity
  and the canonical-equality set that absorb a resubmitted invalidation.
- `cpt-cf-usage-collector-adr-record-identity-derivation` — derives the identifier
  that a reference resolves against.
- `cpt-cf-usage-collector-adr-backfill-isolation` — owns the per-path period
  bounds that decide how far back a withdrawal reaches, and on which route.
- `cpt-cf-usage-collector-adr-consistency-contract` — publishes the feed snapshot
  guarantee that rules out a mutation.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-record-invalidation` — the requirement this decision
  realizes.
- `cpt-cf-usage-collector-fr-invalidation-reason-code` — the fourth departure from
  the faithful copy.
- `cpt-cf-usage-collector-fr-idempotency` — the entry carries its own key and rides
  the same dedup contract.
- `cpt-cf-usage-collector-fr-record-identity` — the reference resolves against the
  derived identifier.
- `cpt-cf-usage-collector-fr-ingestion` — the path the entry travels, shared with
  every measurement.
- `cpt-cf-usage-collector-fr-ingestion-authorization` — PDP authorization applies
  to a withdrawal identically.
- `cpt-cf-usage-collector-fr-query-aggregation` — the fold excludes both entries of
  the pair.
- `cpt-cf-usage-collector-fr-backfill` — the route a withdrawal of a closed
  period travels, and the window that bounds it there.
- `cpt-cf-usage-collector-nfr-aggregate-freshness` — the invalidation-propagation
  bound each plugin publishes.
- `cpt-cf-usage-collector-principle-append-only-ledger` — the design principle that
  this decision codifies.
- `cpt-cf-usage-collector-component-ingestion-gateway` — the component that owns
  five of the six rules.
- `cpt-cf-usage-collector-interface-plugin` — the SPI obligation to exclude both
  entries and to recompute, plus the atomic at-most-one check.
- `cpt-cf-usage-collector-seq-invalidate-record` — the sequence realizing the
  withdrawal.
- `cpt-cf-usage-collector-usecase-invalidate-record` and
  `cpt-cf-usage-collector-usecase-report-decrease` — the two use cases this
  primitive serves, one by withdrawing a measurement and one by staying out of its
  way.
