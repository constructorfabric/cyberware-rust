---
status: accepted
date: 2026-08-17
decision-makers: usage-collector spec owners
---

Created:  2026-09-09 by Virtuozzo International GmbH
Updated:  2026-09-10 by Virtuozzo International GmbH

# Historical import on an isolated, origin-marked route

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [A dedicated origin-marked route, isolated from live ingestion](#a-dedicated-origin-marked-route-isolated-from-live-ingestion)
  - [One ingestion path with wide backdating](#one-ingestion-path-with-wide-backdating)
  - [An operator bulk loader writing to the storage plugin directly](#an-operator-bulk-loader-writing-to-the-storage-plugin-directly)
- [More Information](#more-information)
  - [Prior art](#prior-art)
  - [Why the three bounds are asymmetric](#why-the-three-bounds-are-asymmetric)
  - [Why the floor is a sum, and why it carries no exemption](#why-the-floor-is-a-sum-and-why-it-carries-no-exemption)
  - [The cost to an operator](#the-cost-to-an-operator)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-backfill-isolation`

## Context and Problem Statement

Two retroactive pressures meet on the ingestion path. Usage sometimes has to be
imported for periods already past, when a meter is onboarded late or an emitter
was down. An emitter can also submit a covered period dated into the future,
whether by clock skew or by defect.

Both pressures need a bound. An unbounded retroactive reach places an unbounded
recomputation obligation on any materialised aggregate, and a bulk import competes
with live traffic for the same capacity.

The question is where historical import belongs. It can share the live ingestion
path, run on its own isolated route, or bypass the gear through a plugin-side
loader.

## Decision Drivers

- `cpt-cf-usage-collector-fr-backfill` — a dedicated bulk-import path with a
  bounded backfill window, 90 days by default. It also requires an origin marker
  on every imported entry, workload isolation from live ingestion, and validation
  identical to the live path but for the covered-period bounds it replaces.
- `cpt-cf-usage-collector-fr-live-future-time-bound` — the live path must bound
  the covered period on both sides. It rejects a period ending further into the
  future than a configurable tolerance, 5 minutes by default. It also rejects one
  ending further into the past than a second tolerance, 48 hours by default.
- `cpt-cf-usage-collector-fr-billing-retention-floor` — the retention floor is the
  backfill window plus one replay horizon on every meter, so the two decisions
  cannot drift apart.
- `cpt-cf-usage-collector-fr-record-invalidation` — an invalidation carries the
  period it withdraws, so the path's bounds govern it as they govern a
  measurement.
- `cpt-cf-usage-collector-fr-idempotency` — an imported entry arrives with its
  dedup horizon already partly spent, because retention runs from the covered
  period rather than from import.
- `cpt-cf-usage-collector-fr-rate-limiting` — ingestion quotas apply per calling
  gear, and per calling gear and tenant pair, across every ingestion path
  including backfill.
- `cpt-cf-usage-collector-nfr-workload-isolation` — a bulk workload must not
  degrade ingestion p95 latency.
- `cpt-cf-usage-collector-nfr-throughput-profile` — the load envelope a
  concurrent-load confirmation test runs against.

## Considered Options

- A dedicated origin-marked route, isolated from live ingestion — historical
  import runs on its own bulk-import path. Every entry it accepts carries an
  origin marker, persisted and returned on every read path.
- One ingestion path with wide backdating — a single ingestion path accepts both
  live and historical entries. How far back the covered period reaches is the only
  thing that separates them.
- An operator bulk loader writing to the storage plugin directly — an operator
  tool writes historical entries straight to the active storage plugin. The path
  bypasses the gear.

## Decision Outcome

Chosen option: "A dedicated origin-marked route, isolated from live ingestion". It
bounds both pressures without forcing the live path to accept an unbounded reach.

Historical import runs on a dedicated route, isolated from live ingestion at the
gear. The Ingestion Gateway owns both routes and applies identical validation on
each, but for the covered-period bounds it replaces. Every imported entry carries
an origin marker recording the path it arrived on, and that marker appears on
every read path.

The live path bounds the covered period on both sides
(`cpt-cf-usage-collector-fr-live-future-time-bound`). It refuses a period ending
further into the future than a configurable tolerance, 5 minutes by default. It
refuses a period ending further into the past than a second tolerance, 48 hours
by default. That rejection names the dedicated backfill route as the path such a
submission belongs on.

Each of the three bounds is stated as *further than* its tolerance, so each
admits its own boundary: a covered period ending exactly at the future tolerance,
exactly at the live past tolerance, or exactly at the backfill window is accepted.
Only a period past the value is refused. The comparison reads the end of the
covered period, so the admissible test on the backfill route is
`now - window_end <= backfill_window`.

The backfill window, 90 days by default, bounds the route rather than a kind of
entry. Both retroactive directions travel it: an import moves usage in, a
withdrawal takes it out. Widening the window moves both together.

The retention floor keeps the window a full replay horizon inside retention, on
every meter without exemption
(`cpt-cf-usage-collector-fr-billing-retention-floor`), so an entry imported at the
far edge of the window is still deduplicated and still replayable. Retention must
be at least the window plus one replay horizon, not merely wider than the window:
a 120-day window under 125-day retention leaves 5 days of replay where 35 are
promised. Widening the window raises that floor, so retention is rechecked against
the new floor and raised with it.

**The covered-period bounds are a property of the path, not of the entry kind.**
An invalidation copies its target's period, and that period is checked exactly as
a measurement's own is (`cpt-cf-usage-collector-fr-record-invalidation`). A
withdrawal on the live path reaches 48 hours back, and one reaching further
travels the backfill route for its 90 days.

Both entries of a pair carry one period, so one bound still governs both: the one
the route the withdrawal travelled applies. A target aged past the backfill window
can therefore no longer be withdrawn at all — the same horizon past which no
import lands, and what leaves a closed aggregate stable.

**The window is a hard bound.** No surface reaches past it, and v1 carries no
override. That is what makes the two statements above absolute rather than
conditional. It also settles a third: the covered period is what the dedup
identity carries and what this bound checks, so a period old enough for its entry
to have been purged is refused here, before deduplication is consulted
(`cpt-cf-usage-collector-adr-mandatory-idempotency`). An override was reserved in
earlier revisions of this decision and is withdrawn. It carried no permission, no
error and no bound of its own, and it was the only construct able to place a
submission outside its own idempotency horizon — which is a contract question
rather than an authorization one, and is not answered by granting a role.

### Consequences

- An imported entry arrives with its idempotency horizon already partly spent,
  because retention runs from the covered period rather than from the moment of
  import. The retention floor still leaves it a full replay horizon of
  deduplication, 35 days at the launch defaults.
- Re-running an import is safe wherever the import is still possible. The window
  bounds an admissible covered period strictly inside the retention floor, so a
  re-run either lands and is deduplicated on the dedup identity
  (`cpt-cf-usage-collector-adr-mandatory-idempotency`), or is refused on the
  bound. No re-run reaches a state in which the gear admits a silent duplicate.
- The origin marker lets a consumer tell imported history from live consumption.
  This matters when a charge has already been raised for a period. A consumer that
  rates the feed handles a backfilled entry as batch catch-up rather than as
  current consumption.
- A covered period of any length can be emitted on the live path, if that period
  ended inside the past tolerance. A monthly accrual meter emits there as soon as
  its period closes, and its entry reads `origin = live`.
- Workload isolation for the backfill route is a gear-level obligation. Backend
  pool isolation stays a plugin deployment obligation. That isolation covers the
  bulk withdrawal an emitter defect produces.
- The backfill route is exposed on the SDK trait as well as on REST. It is the
  only route reaching past the live past bound, and a defect is normally found
  days later rather than hours later. Confining it to REST would turn every such
  correction into an operator escalation.
- The ingestion quota is not isolated. It applies per calling gear, and per
  calling gear and tenant pair, across every path. A bulk import therefore spends
  the same allowance as live emission.
- An emitter recovering from an outage longer than the live past bound moves its
  catch-up to the backfill route. Late live data inside that bound still works on
  the path the emitter already calls, and the rejection names the route for
  anything older.
- An emitter that discovers a gap older than the configured window cannot close
  it at all. No surface admits the period, so the remedy is operational rather
  than a caller's: the window is widened before the import, and retention is
  raised with it to hold the floor.

### Confirmation

- A test asserting each bound admits its own boundary and refuses one instant
  past it, on REST and on the SDK trait alike: a covered period ending exactly at
  the future tolerance, exactly at the live past tolerance, and exactly at the
  backfill window.
- A test asserting the live path rejects a covered period ending beyond the future
  bound.
- A test asserting the live path admits a covered period longer than the past
  tolerance, whose end falls inside the tolerance, stamped `origin = live`.
- A test asserting the live path rejects a covered period ending beyond the past
  bound, with an error naming the backfill route. The same test asserts that the
  backfill route admits that period inside its own window.
- A test asserting an invalidation copying a period older than the live past
  bound is rejected on the live path with an error naming the backfill route, and
  accepted on that route carrying the origin marker.
- A test asserting an imported entry carries its origin marker on every read path.
- A test asserting the backfill route rejects a covered period ending beyond its
  window on REST and on the SDK trait alike, with an error naming the bound and
  no override accepted on either surface.
- A concurrent load test asserting a bulk import does not degrade live ingestion
  beyond the `cpt-cf-usage-collector-nfr-throughput-profile` envelope.

## Pros and Cons of the Options

### A dedicated origin-marked route, isolated from live ingestion

Historical import runs on its own bulk-import path. Every entry it accepts carries
an origin marker, persisted and returned on every read path. The route is isolated
from live ingestion workload and validated identically to the live path, but for
its own covered-period bounds.

- Good, because it bounds the recomputation obligation on materialised aggregates.
  An import job stays inside its own configured window, and no surface reaches
  further back.
- Good, because the bound belongs to the route rather than to the entry kind.
  One rule covers import and withdrawal, the origin marker stays truthful for a
  correction of history, and a bulk withdrawal inherits the same isolation.
- Good, because workload isolation keeps a bulk catch-up job from degrading
  live-path service-level objectives.
- Good, because the origin marker lets a consumer separate imported history from
  live consumption. That distinction matters once a charge has already been raised
  for a period.
- Good, because a hard window bound, held inside the retention the deployment
  guarantees for the type, puts every admissible submission inside its own
  idempotency horizon (`cpt-cf-usage-collector-adr-mandatory-idempotency`). No
  caller and no consumer carries a post-horizon deduplication obligation, because
  no submission reaches past it.
- Neutral, because an operator maintains two integrations, one for live emission
  and one for backfill, each against its own route. Both routes draw on the same
  ingestion quota.
- Neutral, because a withdrawal found after the live past bound costs a route
  switch. The rejection names the route, and both surfaces carry it.
- Bad, because the isolated route is an additional surface to build, document, and
  keep in step with the live path's validation rules.
- Bad, because the hard bound makes usage older than the window unrecoverable.
  Neither an import nor a withdrawal reaches it, and the remedy is anticipatory —
  the window is widened before the import, and retention raised with it —
  rather than an escalation available after the fact. An override reaching past
  the bound is deferred rather than granted, because it would have to settle
  whether re-importing already-charged history is legitimate, and a permission
  does not answer that.

### One ingestion path with wide backdating

A single ingestion path accepts both live and historical entries, distinguished
only by how far back a covered period reaches.

- Good, because it is one surface and one contract, with nothing for an emitter to
  choose between.
- Good, because an emitter needs no knowledge of which route to use. Late data
  works on the path the emitter already calls.
- Bad, because an unbounded retroactive reach places an unbounded recomputation
  obligation on any materialised aggregate.
- Bad, because a bulk import competes with live ingestion for the same backend
  pool, so a catch-up job degrades live metering. Both options share one ingestion
  quota, so workload isolation is the whole of what separates them.

### An operator bulk loader writing to the storage plugin directly

An operator tool writes historical entries straight to the active storage plugin,
bypassing the gear entirely.

- Good, because a one-time platform migration of years of history legitimately
  exceeds any bounded window. A direct loader is also the fastest way to move that
  volume.
- Good, because an operator-run loader consumes no ingestion quota. It therefore
  cannot degrade the live-path service-level objectives.
- Good, because the operation is bounded in time and auditable outside the gear,
  as a one-off operator action rather than a standing path.
- Bad, because it bypasses PDP authorization, identifier derivation, and
  validation. An entry loaded that way is not comparable to an entry the gear
  accepted.
- Bad, because the gear cannot vouch for such an entry on any read path. A
  downstream consumer cannot tell where it came from, and no audit trail runs
  through the gear.

## More Information

### Prior art

Bounded live acceptance with historical import on a separate route is a pattern
surveyed marketplace metering systems already use. Azure Marketplace and AWS
Marketplace both enforce a hard backdating limit measured in hours. Both also
route historical import through a path distinct from live ingestion. That
precedent treats a hard bound as a deliberate constraint rather than a
limitation.

### Why the three bounds are asymmetric

Each of the three bounds protects something different. The live path reaches 5
minutes forward and 48 hours back. The backfill route reaches 90 days back through
its window, and every retroactive entry travels that route to get there — an
import and a withdrawal alike.

The future bound protects against a defective emitter opening a period that does
not yet exist. The past bound keeps the origin marker able to separate imported
history from current consumption: without it, a very old measurement arrives
marked as live. The backfill window protects the recomputation surface.

Making the bound a property of the path keeps those protections intact for a
correction. A withdrawal of a closed period *is* a retroactive touch of history.
Exempting it would re-open the marker gap the past bound exists to close, and
leave the bulk emitter-defect workload on the live path.

All three bounds read the end of the covered period, which is the instant that
makes consumption current or historical. A default of 48 hours covers emitter
outage and retry lag, which is what genuinely late live data is. Anything older is
history, and history belongs on the route that marks it and isolates its load.

### Why the floor is a sum, and why it carries no exemption

Retention is measured from the end of the covered period, not from acceptance.
Under `max(window, replay horizon)` the window swallows the horizon whole, and an
entry imported at the far edge of the window satisfies the floor while being
unreplayable in practice — the arithmetic would hold and the guarantee would not.
Summing is what leaves every entry a full replay horizon from the moment it first
becomes readable.

The floor binds every meter because a narrower scope was never checkable. A
declaration carries the fold, the canonical unit, the metadata surface and the
retention policy, and none of them records whether a charging consumer reads the
meter, so a floor scoped to charging meters rested on an operator classifying by
hand. A meter classified wrong falls back to `retention >= window`, and at
equality an entry imported at the far edge is purge-eligible on arrival: a re-run
finds no counterpart and lands as a second entry under the same derived identifier
(`cpt-cf-usage-collector-adr-mandatory-idempotency`). The uniform floor costs a
diagnostic meter the same margin a charging meter carries, which is the price of a
rule a deployment can verify rather than be trusted to have applied.

### The cost to an operator

Onboarding a late meter is a two-step exercise. The operator registers the type,
then runs the backfill import against its own route. That import reaches only
as far back as the configured window, and it draws on the same ingestion quota
as live emission. A meter whose history predates the window needs that window
widened first, with retention raised to match the new floor.

Correcting old data costs a route switch rather than an escalation. An emitter
resubmits its withdrawals against the backfill route, on REST or the SDK, and the
live-path rejection names that route.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-backfill` — the requirement this decision realizes.
- `cpt-cf-usage-collector-fr-live-future-time-bound` — the two bounds the live
  path enforces, forward and back.
- `cpt-cf-usage-collector-fr-billing-retention-floor` — the floor derived from the
  backfill window plus one replay horizon.
- `cpt-cf-usage-collector-fr-record-invalidation` — the other retroactive
  direction, bounded by the route it travels rather than by its kind.
- `cpt-cf-usage-collector-fr-idempotency` — the partly-spent horizon an imported
  entry carries.
- `cpt-cf-usage-collector-fr-rate-limiting` — the quota shared across every
  ingestion path. A bulk import consumes the same allowance as live emission,
  which is a further reason to run it on a route whose load is isolated.
- `cpt-cf-usage-collector-nfr-workload-isolation` — the NFR this isolation answers
  at the gear.
- `cpt-cf-usage-collector-nfr-throughput-profile` — the envelope the concurrent
  test measures against.
- `cpt-cf-usage-collector-component-ingestion-gateway` — the component that owns
  both routes.
- `cpt-cf-usage-collector-seq-backfill-import` and
  `cpt-cf-usage-collector-usecase-backfill` — the sequence and use case.
