---
status: accepted
date: 2026-09-04
decision-makers: usage-collector spec owners
---

Created:  2026-09-09 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

# Entries are selected by the end of their covered period

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Point containment on the covered-period end](#point-containment-on-the-covered-period-end)
  - [Interval overlap with a second case for a point event](#interval-overlap-with-a-second-case-for-a-point-event)
  - [Point containment on the covered-period start](#point-containment-on-the-covered-period-start)
  - [Containment of the whole covered period](#containment-of-the-whole-covered-period)
- [More Information](#more-information)
  - [Related decisions](#related-decisions)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-window-end-selection`

## Context and Problem Statement

Every read path takes a mandatory time range. The aggregate path, the raw query
path and the Plugin SPI all need one rule that says which entries a range
selects. An entry carries a covered period `[window_start, window_end)`, so the
rule compares a period against a range, and more than one comparison is
defensible.

Three rules are in common use. **Containment** selects an entry only when the
range holds its whole period. **Overlap** selects an entry when the period and
the range share any instant. **Point containment** selects an entry when one
chosen instant of the period falls in the range.

The rule also decides how cheaply a plugin can serve a range. A plugin that
pre-aggregates a meter into fixed buckets serves a range by reading buckets
instead of rows, but only if each entry belongs to exactly one bucket. Overlap
denies it that, because one entry can span many buckets.

## Decision Drivers

- `cpt-cf-usage-collector-fr-usage-windows` — the covered period is the only
  emitter-supplied time attribution, and a point event is a zero-length period.
- `cpt-cf-usage-collector-fr-query-aggregation` — one range yields one number per
  meter, and two readers of one range must agree.
- `cpt-cf-usage-collector-fr-query-raw` — the raw path paginates by keyset over a
  mandatory range.
- `cpt-cf-usage-collector-adr-declared-fold` — a consumer that sums adjacent
  ranges must not count an entry twice.
- `cpt-cf-usage-collector-nfr-query-latency` — a plugin must be able to serve a
  range from a rollup.
- `cpt-cf-usage-collector-adr-pluggable-storage` — the rule binds every backend,
  not one.

## Considered Options

- Point containment on the covered-period end — an entry is selected when
  `window_end` falls in the range.
- Interval overlap with a second case for a point event — the rule this decision
  replaces.
- Point containment on the covered-period start — the mirror choice, on
  `window_start`.
- Containment of the whole covered period.

## Decision Outcome

Chosen option: "Point containment on the covered-period end". An entry is
selected when `from <= window_end < to`. The predicate reads one column and needs
no case for a point event, because a point event has
`window_start == window_end`.

The rule holds on every path that takes a time range: the aggregate path, the raw
query path, and every SPI method. The usage feed is unaffected, because it selects
and orders by acceptance sequence rather than by covered period.

Raw pagination orders by `(window_end, id)`, so the range filter and the page
order read one column.

### Consequences

- A point event needs no rule of its own. A point event at midnight on the first
  of the month is still selected by a query for that month.
- A range partitions the entries. Each entry falls in exactly one range of any
  partition, so a consumer can sum adjacent ranges without counting an entry
  twice.
- An entry whose period ends exactly on the upper bound belongs to the **next**
  range, because the period end is exclusive. `[06-01, 07-01)` is selected by a
  July query, not a June one. Edge-to-edge tiling stays the expected emission
  shape, and the consumer shifts its range forward by one period instead.
- An entry wider than the range is no longer selected. A one-day query returns
  nothing for a one-month entry, so a consumer that queries finer than the
  emission grain gets an empty result rather than a whole coarse entry.
- A plugin can hold a rollup keyed on `window_end` and serve a bucket-aligned
  range from it. It never splits or apportions an entry.
- `LATEST` already reports the entry with the greatest `window_end`. Selection and
  that fold now read one column.
- `window_start` stays on the entry and stays in the identity derivation. No
  selection predicate reads it.

### Confirmation

- An SPI contract test for the rule: a point event on the lower bound is selected,
  an entry whose period ends on the upper bound is not, and an entry wider than
  the range is not.
- An aggregate test asserting that the sum over two adjacent ranges equals the sum
  over their union.

## Pros and Cons of the Options

### Point containment on the covered-period end

An entry is selected when `from <= window_end < to`.

- Good, because one predicate covers every entry shape. There is no second case
  to omit.
- Good, because ranges partition the entries, so adjacent sums add up.
- Good, because a plugin can serve a range from a rollup keyed on one column,
  without splitting an entry.
- Good, because the filter column and the raw page order are the same column, so
  one index serves both.
- Good, because an entry reaches a range only once the period it measures has
  ended. A closed range moves only through late arrival inside the backfill
  window.
- Neutral, because it attributes an entry to the range its end reaches. That is a
  labelling rule an emitter has to know.
- Bad, because an entry wider than the range is invisible to that range.

### Interval overlap with a second case for a point event

An entry with a non-zero-length period is selected when the period overlaps the
range. A zero-length period is selected when its instant falls in the range.

- Good, because a range shows every entry that measured any part of it, whatever
  the emission grain.
- Bad, because two cases must be implemented on every path, and the point case is
  the one an author omits. Overlap alone silently drops every point event sitting
  on a lower bound, which is the month-close boundary.
- Bad, because adjacent sums double count. An entry that spans a seam is counted
  in both ranges.
- Bad, because no exact rollup is possible. An entry can span many buckets, so a
  plugin must scan rows or apportion the quantity, and the gear apportions
  nothing.

### Point containment on the covered-period start

An entry is selected when `from <= window_start < to`.

- Good, because it has the partition and rollup properties of the chosen rule.
- Good, because it attributes a period to the range its start falls in, which
  reads more naturally than the end.
- Bad, because an entry is attributed to a range before the period it measures has
  ended. An entry starting on the last day of June and ending in July lands in
  June, so a June aggregate can still move long after June closed.
- Bad, because `LATEST` resolves by the greatest `window_end`. Selection and that
  fold would read different columns, and a rollup would need both.

### Containment of the whole covered period

An entry is selected only when the range holds its whole period.

- Good, because every selected entry lies fully inside the range.
- Bad, because an entry that crosses a boundary is selected by no range at all.
  Summing every month of a year can lose entries.
- Bad, because the result depends on the range width in a way an emitter cannot
  predict. Widening a range can add an entry that a narrower range excluded for
  reasons unrelated to when it was measured.

## More Information

The rule this decision replaces required two selection cases on every path. It
justified the point case with the month-close boundary: overlap logic drops an
entry sitting exactly on a lower bound. Point containment on the period end keeps
that entry, so the case the old rule protected still holds under one predicate.

This decision takes no position on how a plugin builds a rollup. It removes the
property that made an exact rollup impossible.

### Related decisions

- `cpt-cf-usage-collector-adr-declared-fold` — the folds a range serves, and the
  `LATEST` order this rule now shares.
- `cpt-cf-usage-collector-adr-append-only-invalidation` — an invalidation copies
  its target's covered period, so both entries carry one period end and no range
  selects one without the other.
- `cpt-cf-usage-collector-adr-pluggable-storage` — the SPI that carries the rule
  to every backend.
- `cpt-cf-usage-collector-adr-record-identity-derivation` — the identity that
  keeps both period bounds, whether or not selection reads them.
- `cpt-cf-usage-collector-adr-feed-aggregate-split` — the feed, which orders by
  arrival and is not touched by this rule.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-usage-windows` — the covered period this rule selects
  on.
- `cpt-cf-usage-collector-fr-query-aggregation` — the aggregate path the rule makes
  additive across adjacent ranges.
- `cpt-cf-usage-collector-fr-query-raw` — the raw path whose keyset order the rule
  aligns with its filter.
- `cpt-cf-usage-collector-fr-record-invalidation` — the withdrawn pair that shares
  one period end.
- `cpt-cf-usage-collector-interface-plugin` and
  `cpt-cf-usage-collector-contract-storage-plugin` — the surface and contract
  carrying the predicate to every backend.
- `cpt-cf-usage-collector-nfr-query-latency` — the budget a rollup-served range
  has to fit.
