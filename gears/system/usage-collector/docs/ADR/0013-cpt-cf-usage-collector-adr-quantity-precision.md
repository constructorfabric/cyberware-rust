---
status: accepted
date: 2026-08-17
decision-makers: usage-collector spec owners
---

Created:  2026-09-09 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

# Decimal quantities with a published range every plugin round-trips

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [An exact decimal with a published range and a plugin round-trip obligation](#an-exact-decimal-with-a-published-range-and-a-plugin-round-trip-obligation)
  - [A 64-bit binary floating-point quantity](#a-64-bit-binary-floating-point-quantity)
  - [A scaled 64-bit integer](#a-scaled-64-bit-integer)
  - [Per-type precision, declared alongside the unit](#per-type-precision-declared-alongside-the-unit)
- [More Information](#more-information)
  - [Related decisions](#related-decisions)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-quantity-precision`

## Context and Problem Statement

A `SUM` fold over accrued quantities feeds a charge. The arithmetic behind that
number must be exact and reproducible, or the charge it produces is not
defensible. Two readers of the same entries must compute the same total, and that
total must not drift as it is recomputed.

The gear does not own its storage. Any conforming plugin can hold the quantities a
`SUM` fold reads. Exactness is therefore not a property one backend happens to
have. It is only real when every plugin a deployment can bind meets the same
numeric contract. A quantity that round-trips exactly on one backend and drifts on
another is exact by accident rather than exact.

The question is what a quantity is, numerically, and how far the obligation to
preserve it exactly must reach. Three things follow from that answer: how a
quantity travels on the wire, how it is persisted, and what every storage plugin
must guarantee about the values it accepts.

## Decision Drivers

- `cpt-cf-usage-collector-fr-record-quantity` — a quantity is a finite, signed
  decimal, and the gear publishes the range and precision every plugin
  round-trips.
- `cpt-cf-usage-collector-fr-quantity-semantics` — the gear integrates,
  differentiates, interpolates and re-windows a series on no path.
- `cpt-cf-usage-collector-fr-canonical-units` — a quantity persists and travels in
  one canonical unit, never converted, scaled, or rounded by the gear.
- `cpt-cf-usage-collector-fr-metering-unit-binding` — the unit lives on the type
  declaration rather than on the entry. The numeric type must therefore smuggle no
  unit of its own.
- `cpt-cf-usage-collector-fr-aggregation-fold` — a `SUM` fold over many entries
  stays exact even where it exceeds a single entry's ceiling.
- `cpt-cf-usage-collector-interface-plugin` and
  `cpt-cf-usage-collector-contract-storage-plugin` — the round-trip obligation
  must bind every backend a deployment can select, not one implementation.
- `cpt-cf-usage-collector-constraint-no-business-logic` — the gear records a
  caller-supplied quantity. It never computes, prices, or reinterprets it.
- `cpt-cf-usage-collector-nfr-query-latency` — the numeric type chosen must still
  fit the aggregate pushdown budget.

## Considered Options

- An exact decimal with a published range and a plugin round-trip obligation — a
  quantity is a finite, signed decimal. It has a documented ceiling and precision,
  and every plugin round-trips the full range, both signs, digit for digit.
- A 64-bit binary floating-point quantity — the default numeric type in most
  languages and wire formats, used end to end.
- A scaled 64-bit integer — a fixed number of implied decimal places, with one
  global scale shared by every meter.
- Per-type precision, declared alongside the unit — each GTS type declares its own
  decimal precision next to its canonical unit.

## Decision Outcome

Chosen option: "An exact decimal with a published range and a plugin round-trip
obligation". A quantity is a finite, signed decimal in the canonical unit bound to
its GTS type. It is wire-encoded as a string and never as a floating-point number,
and it is persisted in an exact decimal type.

The gear publishes the range and precision as part of its public contract:
at most 28 significant decimal digits (leading zeros excluded) and at most 28
digits after the decimal point. The magnitude is therefore below 10^28, and the
smallest non-zero value is 1×10^-28. Every plugin round-trips that full range
digit for digit, **including its negative half**. A plugin that cannot is unfit
rather than merely limited.

The published bound is deliberately narrower than the exact-decimal carrier the
gear uses, which reaches 29 significant digits. Where the two disagree, the
published bound is normative: the contract does not inherit a carrier's own
ceiling, so a plugin built on a different exact-decimal type is not held to one.
The wire patterns bound the integer and fraction parts separately and cannot
express the significant-digit bound, so they are a necessary but not sufficient
check — the gear enforces the full bound at ingestion.

The published range binds a single entry, never a fold over many. An aggregate
result therefore carries a **numeric type distinct from a quantity**: unbounded,
with no ceiling of its own, and a separate schema on the contract rather than a
reuse of the quantity schema. A `SUM` wider than the per-entry ceiling is
returned exactly — it never saturates, overflows, or fails to encode. It is
wire-encoded as a string on the same float-safety grounds.

The sign of a quantity is never constrained at ingestion, and the gear
differentiates no series on any path.

### Consequences

- The negative half of the range is called out explicitly, because a plugin author
  is most likely to overlook it. An invalidation entry echoes its target's
  quantity, so both signs traverse the same persistence path, not only the
  positive one an author tests first.
- The range is a contract. Widening it later is a breaking change for every plugin
  rather than an additive one. A plugin already built to the published ceiling has
  no way to hold a value beyond it.
- The unit lives on the type declaration rather than on the quantity. A quantity
  read without resolving its type is uninterpretable, whatever its numeric value.
- A meter whose consumption is naturally a level must be pre-integrated at the
  emitter. It carries an accrued quantity in an accrued unit, because the gear
  does not integrate.
- A plugin needs a second numeric read path. A backend whose native fold is
  already unbounded still decodes into the capped quantity type by default, which
  turns a legitimately wide `SUM` into an internal error rather than a result.
  Reading a fold into the aggregate type is an obligation, not an optimization.
- Recording a caller-supplied quantity is recording rather than computing. That is
  what keeps this decision inside the no-business-logic constraint.

### Confirmation

- An SPI conformance test that round-trips the full published range, including its
  negative half.
- An aggregate test asserting that a `SUM` wider than the per-entry ceiling is
  returned exactly.
- A wire test asserting that a quantity is encoded as a string rather than a
  number.

## Pros and Cons of the Options

### An exact decimal with a published range and a plugin round-trip obligation

A quantity is a finite, signed decimal with a documented ceiling and precision.
Every plugin round-trips the full range, both signs, digit for digit.

- Good, because a `SUM` fold stays exact. The arithmetic a charge derives from
  does not drift, however many entries feed it.
- Good, because the range and precision are published as concrete figures. A
  plugin author has a verifiable target rather than an assumption to guess at.
- Good, because a wide `SUM` escalates to an arbitrary-precision result without
  changing the per-entry contract. The per-entry ceiling and the aggregate ceiling
  can differ on purpose.
- Neutral, because the round-trip obligation is a conformance burden every plugin
  carries. One contract test verifies it rather than leaving it assumed.
- Bad, because a backend whose native decimal type is narrower than the published
  range is disqualified outright, not merely constrained to a smaller domain.

### A 64-bit binary floating-point quantity

The default numeric type in most languages and wire formats, used end to end.

- Good, because it needs no special encoding. A JSON number carries it directly,
  in any client language.
- Good, because it is fast and compact, with hardware support on every target
  platform.
- Bad, because a `SUM` fold must be exact, and binary floating point cannot
  guarantee that at scale.
- Bad, because the rounding error is silent, accumulates across a fold, and lands
  in money.

### A scaled 64-bit integer

A fixed number of implied decimal places, with one global scale shared by every
meter.

- Good, because the arithmetic is exact. An integer has no rounding error of its
  own.
- Good, because it is cheap to store and compute, and portable across every
  backend without a decimal library.
- Bad, because it fixes one scale globally, and meters do not share one scale. A
  byte counter and a fractional credit unit cannot both fit one implied precision.
- Bad, because it overflows on a wide `SUM`. A 64-bit integer has no room to widen
  the way an arbitrary-precision aggregate does.

### Per-type precision, declared alongside the unit

Each GTS type declares its own decimal precision next to its canonical unit.

- Good, because each meter gets exactly the precision it needs, neither more nor
  less.
- Neutral, because precision becomes a per-type declared attribute, the same shape
  as the canonical unit it sits beside.
- Bad, because the round-trip obligation stops being verifiable in a single
  contract test. Each declared precision needs its own case.
- Bad, because a plugin's fitness then depends on which types a deployment happens
  to declare, rather than on one fixed contract every deployment shares.

## More Information

This decision takes no position on rounding for money. The gear does not price, so
it has no rounding rule to state. Rounding belongs to the downstream rating
consumer, and the gear's obligation stops at handing that consumer the quantity it
was given, unchanged.

### Related decisions

- `cpt-cf-usage-collector-adr-pluggable-storage` — the SPI that carries the
  round-trip obligation to every backend.
- `cpt-cf-usage-collector-adr-declared-fold` — the `SUM` fold whose exactness this
  numeric type supports.
- `cpt-cf-usage-collector-adr-registry-owned-typing` — the declaration that binds
  the canonical unit a quantity is expressed in.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-record-quantity` — the requirement this decision
  realizes.
- `cpt-cf-usage-collector-fr-quantity-semantics` — the prohibition on integrating
  or differentiating.
- `cpt-cf-usage-collector-fr-canonical-units` — the unit set a quantity is
  expressed in.
- `cpt-cf-usage-collector-fr-metering-unit-binding` — the binding of that unit to
  the type declaration.
- `cpt-cf-usage-collector-fr-aggregation-fold` — the fold whose exactness the
  numeric type must support.
- `cpt-cf-usage-collector-interface-plugin` and
  `cpt-cf-usage-collector-contract-storage-plugin` — the surface and contract
  carrying the round-trip obligation.
- `cpt-cf-usage-collector-constraint-no-business-logic` — recording a
  caller-supplied quantity is recording, not computing.
- `cpt-cf-usage-collector-nfr-query-latency` — the pushdown budget an exact
  decimal fold must fit.
