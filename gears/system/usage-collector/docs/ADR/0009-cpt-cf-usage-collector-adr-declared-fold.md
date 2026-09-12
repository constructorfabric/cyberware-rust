---
status: accepted
date: 2026-08-17
decision-makers: usage-collector spec owners
---

Created:  2026-09-09 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

# The aggregation fold is declared on the type, not chosen per query

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Additivity is normative](#additivity-is-normative)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [One immutable fold declared on the type](#one-immutable-fold-declared-on-the-type)
  - [Caller-chosen aggregation operation per query](#caller-chosen-aggregation-operation-per-query)
  - [Write-side kind classification, counter against gauge](#write-side-kind-classification-counter-against-gauge)
  - [A cumulative fold in the OpenTelemetry style](#a-cumulative-fold-in-the-opentelemetry-style)
- [More Information](#more-information)
  - [Open question — an emitter that cannot difference](#open-question--an-emitter-that-cannot-difference)
  - [Related decisions](#related-decisions)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-declared-fold`

## Context and Problem Statement

A meter must yield one number for a period. Two consumers reading the same
entries can otherwise obtain two defensible answers, and neither can then say
which answer a charge derived from. That is a correctness problem rather than a
presentation problem, because the number leaves the gear and becomes money.

Every entry references a GTS type, and the question is where the aggregation
function lives. Three homes are candidates. The function can live in the request,
in a write-side classification of the meter, or in the type declaration.

Two constraints bear on the answer. The gear integrates, differentiates,
interpolates and re-windows on no path
(`cpt-cf-usage-collector-fr-quantity-semantics`), so a fold that needs any of
those is unavailable to it. A plugin that serves the aggregate path from a
pre-computed representation must also know what to accumulate at declaration time
(`cpt-cf-usage-collector-principle-aggregate-asymmetry`).

## Decision Drivers

- `cpt-cf-usage-collector-fr-aggregation-fold` — a meter yields one number for a
  period, and the fold is what makes that number determinate.
- `cpt-cf-usage-collector-fr-query-aggregation` — the aggregate path serves one
  GTS type per request, so the fold is resolvable before dispatch.
- `cpt-cf-usage-collector-fr-quantity-semantics` — the gear integrates and
  differentiates on no path, so the fold set can hold only folds computable from
  the stored quantities.
- `cpt-cf-usage-collector-fr-usage-type-declaration` — the attributes that give a
  persisted entry its meaning are immutable, and the fold is one of them.
- `cpt-cf-usage-collector-fr-record-quantity` — the ingestion contract carries no
  per-meter case, and the sign of a quantity is never constrained.
- `cpt-cf-usage-collector-fr-billing-usage-feed` — the acceptance sequence is
  strictly monotonic per tenant and type, which a selecting fold needs for a
  total tie-break.
- One declaration rather than two — additivity and the fold must not be separate
  declared facts that can disagree.

## Considered Options

- One immutable fold declared on the type — the declaration carries exactly one
  fold from a closed set. Every read of that meter applies it.
- Caller-chosen aggregation operation per query — the aggregate request names the
  operation, and the same entries serve any operation the caller asks for.
- Write-side kind classification, counter against gauge — the declaration carries
  a kind and the ingestion path validates against it. A per-kind matrix fixes the
  admissible operations.
- A cumulative fold in the OpenTelemetry style — the meter reports a running
  total, and the gear differences consecutive readings to obtain period
  consumption.

## Decision Outcome

Chosen option: "One immutable fold declared on the type". Every GTS type declares
exactly one aggregation fold, drawn from the closed set `SUM`, `COUNT`, `MAX`,
`MIN`, `LATEST`. The declared fold is immutable for the type's lifetime, as a case
of the declaration immutability rule
(`cpt-cf-usage-collector-fr-usage-type-declaration`).

The fold is a property of the type. The gear resolves it through the entry's type
reference. It is never carried per entry, and the gear never infers it from the
shape of the type identifier.

The aggregate query carries no aggregation parameter. The path serves the declared
fold and no other (`cpt-cf-usage-collector-fr-query-aggregation`). The Query
Gateway resolves the declaration through the Type Resolver and pushes the declared
fold down to the active plugin.

The ingestion path never consults the fold. No ingestion invariant depends on it,
and the fold does not constrain the sign of a quantity
(`cpt-cf-usage-collector-fr-record-quantity`).

The closed set is as follows.

| Fold     | The quantity of an entry is                                          |
| -------- | -------------------------------------------------------------------- |
| `SUM`    | the amount accrued over the period the entry covers                  |
| `COUNT`  | not read — the fold counts entries, and a withdrawn pair counts none |
| `MAX`    | one observation, and the fold reports the largest in range           |
| `MIN`    | one observation, and the fold reports the smallest in range          |
| `LATEST` | one observation, and the fold reports the one with the greatest covered-period end in range, ties broken by the greatest acceptance sequence |

**The quantity under `COUNT`.** A `COUNT` meter's entries still carry a quantity,
because the entry shape is uniform across every type. The quantity means nothing
there. Under `COUNT` one entry is one event, and the fold reads only how many
entries there are. An emitter sends `1` by convention, and a consumer must not
read a `COUNT` meter's quantity as a measurement. Ingestion enforces neither part
of that convention, because it never consults the fold.

**What `COUNT` counts.** It counts the measurement entries in range that no
accepted invalidation withdraws. It never counts an invalidation entry, and it
never counts the entry one withdraws. That is the fold exclusion
(`cpt-cf-usage-collector-adr-append-only-invalidation`) read under `COUNT`: a
withdrawn measurement counts 1 and its invalidation counts 1, so a fold admitting
both returns 2 where the answer is 0.

**Ties under `LATEST`.** Two entries of one meter can share a covered period,
because an idempotency key distinguishes them as well as a period. `LATEST`
selects an entry rather than reducing values. An undefined tie therefore lets two
consumers read the same range and obtain different quantities.

The order is total for that reason: the greatest covered-period end, then the
greatest acceptance sequence. No two entries of one meter share an acceptance
sequence, because that sequence is strictly monotonic per tenant and type
(`cpt-cf-usage-collector-fr-billing-usage-feed`). An aggregation group always sits
inside one such scope, so the second key always resolves the tie. `MAX` and `MIN`
need no such rule, because a tie there returns the same value whichever entry
supplied it.

### Additivity is normative

This rule binds a consumer that reads entries directly, through the raw query
path or the feed. Such a consumer derives period consumption by summing
quantities only where the declared fold is `SUM`. It leaves out any entry
withdrawn by an accepted invalidation, along with the invalidation entry itself.

Under every other fold the quantities are observations rather than accrued
amounts, and summing them is invalid. This rule is the whole of what a consumer
needs in order to fold a meter correctly. It is also why a consumer that never
calls the aggregate path still resolves the declaration.

### Consequences

- Only `SUM` yields a chargeable period quantity. `MAX`, `MIN` and `LATEST` are
  descriptive: they characterise a series without producing an amount consumed
  over a period.
- A meter whose consumption is naturally a level must therefore be pre-integrated
  at the emitter. Stored volume is the first case to hit this. Such a meter
  carries an accrued quantity in an accrued unit, such as `byte-hours`, and
  declares `SUM`.
- The set is closed. Adding a fold later is an additive change, and removing one
  is breaking.
- The excluded folds are excluded for a stated reason. An unweighted mean over
  irregularly spaced observations resembles the time-weighted mean that money
  derives from, without being it. A distinct count is not re-aggregatable
  without either a mergeable sketch in every rollup or a full scan.
- Additivity follows from the fold. There is one attribute to declare and one to
  read, rather than two that can disagree.
- A meter declared with the wrong fold is a new GTS type rather than an edit in
  place, because the fold is immutable.
- One meter answers one question on the aggregate path. A consumer that needs a
  second view of the same series reads raw entries and folds them itself.

### Confirmation

- A contract test per fold, asserting the aggregate result that the declaration
  selects.
- A `LATEST` tie-breaking test that pins the total order over the covered-period
  end and the acceptance sequence.
- A test asserting that the aggregate request carries no operation parameter.
- A test asserting that ingestion accepts an entry without consulting the
  declared fold.
- A `COUNT` test over a range holding a withdrawn measurement, asserting that the
  aggregate returns 0, and that folding the raw rows of the same range by hand
  returns the same number.

## Pros and Cons of the Options

### One immutable fold declared on the type

The declaration carries exactly one fold from a closed set. Every read of that
meter applies it, and no caller can select another.

- Good, because a period yields one number per meter. A charge then derives from a
  determinate value, and two consumers cannot disagree about it.
- Good, because the aggregate request loses a parameter, and with it a class of
  request that is well-formed and semantically wrong.
- Good, because additivity follows from the fold. One declared attribute replaces
  two that can disagree.
- Good, because every fold in the set re-aggregates from a rollup. A plugin can
  accumulate a rollup at the declared granularity and re-aggregate to any
  coarser group, provided each bucket keeps the state its fold needs: a sum
  under `SUM`, a count under `COUNT`, the extreme value under `MAX` and `MIN`,
  and the selected quantity with its covered-period end and acceptance sequence
  under `LATEST`. A bucket that keeps quantities alone answers a coarser query
  wrongly under `COUNT` and `LATEST`.
- Good, because the ingestion path stays fold-blind. Every emitter uses one retry
  pattern and one validation contract across all the types it emits.
- Neutral, because the set admits accrued amounts and single observations only. An
  emitter of a level series must pre-integrate before submission.
- Bad, because the fold is immutable. A mis-declared meter costs a new GTS type
  and a re-emission, rather than an edit.

### Caller-chosen aggregation operation per query

The aggregate request names the operation, and the same stored entries serve
whichever operation the caller asks for.

- Good, because it gives a read consumer maximum flexibility. One meter serves
  several questions without a second declaration.
- Good, because a new question over an existing meter needs no registry change at
  all.
- Neutral, because the declaration carries no fold, so registering a meter is
  marginally cheaper.
- Bad, because two consumers reading the same entries can compute two different
  numbers, and both are defensible. No number is then authoritative enough to
  charge against.
- Bad, because a request can be well-formed and semantically wrong. A `SUM` over
  an observation series returns a figure that is wrong by orders of magnitude
  rather than imprecise.
- Bad, because a pre-aggregating plugin cannot know what to accumulate. It must
  keep state for every operation a caller can ask for, or scan raw entries.

### Write-side kind classification, counter against gauge

The declaration carries a kind, the ingestion path validates each entry against
it, and a per-kind matrix fixes the admissible operations.

- Good, because the ingestion path can validate a sign per kind. A counter delta
  submitted as a negative quantity is rejected at the gateway.
- Neutral, because the kind is a coarse hint that groups meters for a consumer
  reading a catalog.
- Bad, because additivity and the fold then become two declared things that can
  disagree. A kind says how quantities relate, and the chosen operation says what
  a read does with them.
- Bad, because the Ingestion Gateway takes on a kind-dependent case for no gain.
  It must resolve the classification and branch on it before it accepts an entry.
- Bad, because a per-kind admissible operation matrix is a second surface to
  maintain, and it still does not produce one number per period.
- Bad, because sign validation per kind forbids a legitimate negative
  measurement. The gear records a real decrease in consumption as an ordinary
  entry with a negative quantity.

### A cumulative fold in the OpenTelemetry style

The meter reports a running total on each emission. The gear differences
consecutive readings to obtain the consumption of a period.

- Good, because an emitter that can read only a running total submits its reading
  unchanged, with no state of its own.
- Good, because it matches the OpenTelemetry cumulative temporality, so an
  exporter built on that model needs no conversion.
- Bad, because the gear must differentiate, and
  `cpt-cf-usage-collector-fr-quantity-semantics` forbids differentiating on every
  path.
- Bad, because the gear must detect a reset. A restarted emitter drops its running
  total, and the entry shape carries no reliable reset signal.
- Bad, because a wrong reset decision is silent and unbounded. It either drops a
  period of consumption or charges the whole lifetime total once more.

## More Information

### Open question — an emitter that cannot difference

The fold set admits accrued amounts and single observations, and nothing else. An
emitter able to report only a running total fits neither, and the gear requires
it to difference its own readings before submission. A cumulative fold costs two
things the gear refuses: it relaxes the prohibition on differentiating, and it
takes on reset detection. The exclusion is therefore deliberate.

This is a conscious divergence from the OpenTelemetry data model, not an
oversight. The trigger for revisiting it is an emitter that genuinely cannot
difference. This decision does not settle that question, and the PRD carries it
as open.

### Related decisions

- `cpt-cf-usage-collector-adr-registry-owned-typing` — places the declaration that
  carries the fold in `types-registry`.
- `cpt-cf-usage-collector-adr-quantity-precision` — fixes what a quantity is,
  independent of the fold that reads it.
- `cpt-cf-usage-collector-adr-feed-aggregate-split` — fixes which path a charging
  consumer reads, which is why the additivity rule binds a consumer that never
  calls the aggregate path.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-aggregation-fold` — the requirement this decision
  realizes.
- `cpt-cf-usage-collector-fr-quantity-semantics` — the prohibition on integrating
  or differentiating that closes the fold set.
- `cpt-cf-usage-collector-fr-query-aggregation` — the path that serves the
  declared fold and no other.
- `cpt-cf-usage-collector-fr-record-quantity` — what a quantity must satisfy,
  independent of the fold.
- `cpt-cf-usage-collector-fr-usage-type-declaration` — the declaration carrying
  the fold, and its immutability.
- `cpt-cf-usage-collector-fr-usage-type-resolution` — the resolution that delivers
  the declared fold to every read, and that rejects an unresolvable type before
  dispatch.
- `cpt-cf-usage-collector-fr-billing-usage-feed` — the acceptance sequence that
  makes the `LATEST` tie-break total.
- `cpt-cf-usage-collector-principle-declared-fold` — the design principle that
  this decision codifies.
- `cpt-cf-usage-collector-principle-aggregate-asymmetry` — the read-side asymmetry
  that the declared fold makes safe.
- `cpt-cf-usage-collector-component-query-gateway` — the component that resolves
  the fold before dispatch.
- `cpt-cf-usage-collector-seq-query-aggregated` and
  `cpt-cf-usage-collector-usecase-query-aggregated` — the sequence and use case
  that read the declaration and carry no aggregation parameter.
