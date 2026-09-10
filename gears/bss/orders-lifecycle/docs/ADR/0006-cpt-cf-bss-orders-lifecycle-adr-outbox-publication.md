---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team
---

# ADR-0006: Events Are Published Asynchronously From An Outbox

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Transactional outbox with an asynchronous drain (chosen)](#transactional-outbox-with-an-asynchronous-drain-chosen)
  - [Synchronous publish inside the transaction](#synchronous-publish-inside-the-transaction)
  - [Publish after commit, in the same request](#publish-after-commit-in-the-same-request)
  - [Change data capture off the write-ahead log](#change-data-capture-off-the-write-ahead-log)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-lifecycle-adr-outbox-publication`

## Context and Problem Statement

Eleven order events are a published contract consumed by three sibling gears. Every one of them
originates in a transition that also writes the aggregate, a version row and an audit entry inside
one database transaction.

PRD §7.1 sets the transition threshold as `p95 < 1 s` and defines it as **"durable write + event
publish"** — one budget covering both. So the question is not merely how events reach the bus, but
whether publication is part of the commit the PRD is measuring.

Publishing inside the transaction means a network call while a row lock is held. Publishing after
it means a window in which the state changed and no consumer knows. Neither is free, and the
choice determines the failure mode the whole seam inherits.

## Decision Drivers

* A transition holds the aggregate row lock, which also serialises audit-sequence allocation — anything slow inside that lock costs every concurrent caller on the same order.
* "Zero silent drops" (PRD §7.1) has to survive a broker that is unavailable at commit time.
* Per-order ordering is a contract consumers rely on; global ordering is explicitly not offered.
* A consumer must be able to de-duplicate, because any at-least-once delivery will re-deliver.
* Whatever is chosen is load-bearing for three gears and expensive to reverse once they have built against it.

## Considered Options

* **Transactional outbox with an asynchronous drain** — enqueue a row in the transition transaction, publish from a separate worker
* **Synchronous publish inside the transaction** — call the broker before commit
* **Publish after commit, in the same request** — commit, then publish, with a reconciliation sweep for the gap
* **Change data capture off the write-ahead log** — derive events from the database's own replication stream

## Decision Outcome

Chosen option: **transactional outbox with an asynchronous drain**. Exactly one outbox row is
written per event-declaring committed transition, in the same transaction, and a sharded drain
publishes it afterwards. `design/01-foundation.md` §4.4 is normative.

**This decision is the reason PRD §7.1's threshold cannot hold as written**, and that is its most
important consequence rather than an incidental detail. Publication is asynchronous by
construction, so it cannot sit inside a `p95 < 1 s` commit budget; delivery carries its own
**30 s p95** budget instead (`DECISIONS.md` D-41). The combined figure is not achievable by any
implementation of this decision, which is why the divergence is routed to Product as `Q-16` rather
than asserted away. Choosing synchronous publication *would* have satisfied the PRD's wording
literally — at the cost below.

Synchronous publish is rejected because it puts a network call inside the row lock: broker latency
becomes transition latency for every caller on that order, and a broker outage becomes an
order-taking outage. It also cannot be made atomic — the commit and the publish are two systems,
so a failure between them either loses the event or commits a transition nobody can see, which is
the exact defect the outbox exists to remove.

Publish-after-commit-in-request is rejected for the same atomicity gap in a smaller window, plus
it makes every caller pay the publish latency while still needing the reconciliation sweep that
the outbox drain already is.

CDC is rejected because the event payloads are a curated contract, not a row image: `01 §4.4`
requires each event to carry a summary block sufficient for a consumer to act without fetching the
order back, and deriving that from WAL rows would put contract-shaping logic in a replication
consumer outside this gear's boundary.

### Consequences

* **A consumer sees a state change after the commit, not at it.** The delay is bounded by the 30 s delivery budget, not by the transaction. Anything requiring read-your-write consistency must read the order, not wait for the event.
* **Delivery is at-least-once**, so every consumer needs de-duplication. `(order_id, sequence)` gives them the key to do it.
* **Per-order ordering is preserved by construction, and only per-order.** `shard_key` is `hash(order_id) % 64` with a *fixed* bucket count and the drain leases contiguous bucket ranges, so re-tuning parallelism can never split one order across two leaseholders. This is why the modulus is fixed rather than tied to the worker count.
* **An undeliverable event becomes an operational object.** After a bounded attempt count a row is parked as an inspectable dead-letter record with an alert, and an operator re-drive endpoint exists to republish it. That endpoint has no PRD basis and is disclosed as a design-introduced surface (`Q-19`).
* **The outbox is a traffic-driven table**, so it carries a 30-day retention on delivered rows, purged by the drain, and is range-partitioned by month.
* **The audit trail and the event stream can disagree transiently.** An audited transition whose outbox row is undelivered is real and invisible; only the order read reflects it. This is acceptable and is why the read is never served from a replica.

### Confirmation

Verified by the one-outbox-row-per-event-declaring-transition invariant in `01 §3.7`, which names
its own cross-table cardinality test; by `(order_id, sequence)` UNIQUE with the sequence allocated
under the aggregate row lock, so ordering cannot be violated by concurrent drains; by a test
asserting a parked dead-letter row alters no order state; and by the drain-lag metric measured
against the 30 s budget separately from the commit budget, which is the measurement `Q-16` needs in
order to be answered.

## Pros and Cons of the Options

### Transactional outbox with an asynchronous drain (chosen)

* Good, because enqueue and commit are one transaction, so an event can never be lost for a committed transition nor emitted for an uncommitted one.
* Good, because no network call sits inside the row lock.
* Good, because throughput scales with replicas up to the bucket count.
* Bad, because it makes PRD §7.1's combined threshold unsatisfiable and requires a PRD amendment.
* Bad, because consumers must de-duplicate and tolerate delay.

### Synchronous publish inside the transaction

* Good, because it satisfies the PRD's "durable write + event publish" wording literally.
* Good, because a consumer sees the change with no added delay.
* Bad, because broker latency becomes transition latency under the aggregate row lock.
* Bad, because it is not atomic across two systems, so the failure between them either drops the event or hides a committed transition.

### Publish after commit, in the same request

* Good, because the lock is released before the network call.
* Bad, because the atomicity gap remains, just smaller.
* Bad, because it still needs a reconciliation sweep, which is an outbox with extra steps.

### Change data capture off the write-ahead log

* Good, because it needs no application write path at all.
* Bad, because the events are a curated contract, not row images, so payload shaping would move outside this gear.
* Bad, because it couples three consumer gears to this gear's physical schema.

## More Information

Superseded by nothing. `DECISIONS.md` D-41 records the sharding and batching shape and the 30 s
budget; D-42 records the per-port deadlines that bound the *other* asynchronous cost. This ADR
exists because the 2026-09-10 review found the publication mode recorded only as a capacity-table
row, with the alternatives argued nowhere — while being the decision that makes a PRD acceptance
criterion unsatisfiable.

## Traceability

- **PRD**: [`../PRD.md`](../PRD.md) — §7.1 transition latency and audit completeness, §12 AC-17
- **DESIGN**: [`../design/01-foundation.md`](../design/01-foundation.md) §3.7 `orders_event_outbox`, §3.8, §4.4

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-lifecycle-fr-order-events` — the eleven events reach three consumer gears through the outbox; this decision fixes the delivery semantics as at-least-once with per-order ordering, and fixes the de-duplication key consumers must use
* `cpt-cf-bss-orders-lifecycle-nfr-order-transition-latency` — the decision removes the publish from the commit path, which is what lets the commit meet its budget, and is simultaneously why the PRD's combined "durable write + event publish" threshold cannot hold (`Q-16`)
* `cpt-cf-bss-orders-lifecycle-nfr-order-audit-completeness` — enqueue shares the transition transaction, so an event-declaring committed transition cannot exist without its outbox row; this is the "zero silent drops" guarantee at the egress boundary
* `cpt-cf-bss-orders-lifecycle-component-transition-engine` — the engine writes the outbox row and allocates its sequence under the aggregate row lock; the drain is a separate worker that mutates no order state
- **Decisions register**: [`../DECISIONS.md`](../DECISIONS.md) — D-41, D-42, Q-16, Q-19
