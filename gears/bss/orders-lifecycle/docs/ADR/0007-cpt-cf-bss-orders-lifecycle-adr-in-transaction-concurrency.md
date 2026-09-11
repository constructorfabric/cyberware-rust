---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team
---

# ADR-0007: Concurrency Rules Are Database Constraints Inside The Transition Transaction

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [A claim table with a partial unique index (chosen)](#a-claim-table-with-a-partial-unique-index-chosen)
  - [Application-level counting under the aggregate row lock](#application-level-counting-under-the-aggregate-row-lock)
  - [`SERIALIZABLE` isolation for transitions touching an overlap key](#serializable-isolation-for-transitions-touching-an-overlap-key)
  - [A PostgreSQL advisory lock on the hashed key](#a-postgresql-advisory-lock-on-the-hashed-key)
  - [An upstream reservation in Subscriptions](#an-upstream-reservation-in-subscriptions)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-lifecycle-adr-in-transaction-concurrency`

## Context and Problem Statement

PRD §6.1(g) requires **at most one in-flight order per overlap key**: a second submit against the
same key while one is in flight must be rejected. This is not the same rule as the
concurrent-*subscription* cardinality of §6.1(f), which Catalog or Contract may configure — this
one is fixed at one, because it is a concurrency-correctness rule rather than a commercial policy.

The gate evaluates it as a predicate. But the gate resolves its inputs **outside** the transition
transaction, before the lock is taken, so two identical submits arriving together both read "no
order in flight" and both proceed. The original design did exactly this and both passed
(`DECISIONS.md` D-26, review finding R-30) — two orders provisioning the same product for the same
payer.

An idempotency key does not help: it protects against the same call twice, not two distinct
orders carrying identical content.

## Decision Drivers

* A predicate evaluated outside the transaction is a read of stale state by the time the transaction commits, however carefully it is written.
* Discovering the collision at activation instead means entering the compensation path, which the design elsewhere calls the most expensive and least reliable path it has.
* The enforcement must survive a later author who reads the gate predicate, judges the constraint redundant, and removes it.
* The rule's target state is mutable — a claim is released on a terminal transition — while the order line the key is resolved from is append-only.
* Whatever enforces it runs on the submit hot path, inside the lock, so its cost is paid by every order.

## Considered Options

* **A claim table with a partial unique index**, written inside the transition transaction
* **Application-level counting under the aggregate row lock** — read the in-flight set, decide, write
* **`SERIALIZABLE` isolation** for the transitions that touch an overlap key
* **A PostgreSQL advisory lock** on the hashed overlap key for the duration of the transaction
* **An upstream reservation** — ask Subscriptions to reserve the key before submit

## Decision Outcome

Chosen option: **a claim table with a partial unique index**. `orders_inflight_overlap_claim`
carries one row per resolved line key, with `partial UNIQUE (payer_tenant_id, overlap_scope_key)
WHERE released_at IS NULL`, inserted inside the transition transaction. A collision maps to the
registered `order-in-flight-for-key` refusal. The gate predicate remains as a **friendly
pre-check** that produces a readable refusal in the common case; it is explicitly **not** the
enforcement.

**How the collision is detected is a correctness requirement, not a detail, and `01 §3.6` now
states it.** A raw unique violation in PostgreSQL puts the whole transaction into an aborted
state: no further statement in it can succeed and only a rollback is accepted. But the refusal
this decision needs is a *committed* outcome under `ADR/0005` — it has to append an audit entry
and settle its idempotency record and commit, and those writes cannot happen in a transaction the
violation has already aborted. Two things resolve it, both normative in
[`../design/01-foundation.md`](../design/01-foundation.md) **§3.6** *Attempt Transition*
(`DECISIONS.md` D-86). The claim is acquired with `ON CONFLICT … DO NOTHING` and the collision
detected as a **row shortfall** rather than raised as an error, so the transaction is never
aborted and stays usable for the audit append and the settle. `DO NOTHING` buys that at the cost of
**per-row** semantics: offered a free key and a taken one together it inserts the free key and skips
the taken one, so the shortfall branch is reached with rows already inserted. Acquisition is
therefore required to be **all-or-none** — the shortfall branch **MUST** delete the rows the same
statement returned before it settles and commits the refusal, and a test **MUST** cover one
conflicting key and one free key in one acquisition. Without it the refused order keeps a live claim
on the free key and has **no transition of its own to release it**: a refused submit leaves the order
in `draft`, and 17.1's terminal release does not run until an auto-void or a cancel eventually
fires, so the key stays blocked for every other order in between — the rule's enforcement producing
the collision the rule exists to prevent. The unwind needs no savepoint: the insert returns the rows
it made, the transaction is still usable, and the delete is an ordinary statement in it. And
acquisition is **step 17**,
ahead of the version append and every other contribution, so a refusal has nothing durable to
unwind: had it run later, the refusal would have to discard a committed version row and a moved
current-version pointer in tables that grant no DELETE. **There is consequently no savepoint
anywhere in the algorithm** — an earlier version of this ADR pointed at one, and ordering replaced
it. Three properties the acquisition depends on are stated with it: keys offered distinct, keys
offered in a total order against deadlock, and READ COMMITTED isolation (under snapshot isolation
the insert raises a serialisation failure instead of reporting a shortfall). §3.7 declares the
constraint itself, and carries **no foreign key** to `orders_order_version` for the same ordering
reason.

A separate table is needed because the constraint's subject is mutable — `released_at` is set on a
terminal transition, in `01 §3.6` sub-step **17.1**, which runs ahead of the acquisition branch
precisely because a terminal row carries no resolved keys and a release placed inside that branch
would never run — while `orders_order_line`, where the key is resolved, is append-only and
version-scoped. A partial unique index cannot be built over a state that lives on a different row.

Application-level counting is rejected because it moves the guarantee from a constraint the
database enforces to logic a future refactor can weaken, and because it would have to lock
something — the aggregate row lock does not help, since the colliding orders are *different*
aggregates.

`SERIALIZABLE` is rejected because it pays a global cost for one local rule and converts the
collision into a serialisation failure with no registered reason, which the caller cannot act on.

An advisory lock is rejected because it is not durable: it disappears on connection loss, so it
cannot express a claim that must survive for the whole in-flight window — hours or days, not
milliseconds.

Upstream reservation is rejected because it needs a capability Subscriptions does not expose, and
it would make order submission depend on a second system's write availability.

### Consequences

* **The enforcement is a constraint, so it cannot be bypassed by a code path.** This is the property the decision exists for: no slice, migration or admin surface can admit a second in-flight order on a key.
* **The refusal is not free, and it is the sharp edge of this decision.** The collision is detected by the database mid-transaction, so it must be both *mapped* to `order-in-flight-for-key` and *observed without losing the transaction*. Two failures are possible and they are different: an unmapped violation surfaces as a 500 rather than one of the exhaustive outcomes, and a violation allowed to abort the transaction leaves the audit entry and the settled idempotency record unwritten — a silent drop of exactly the kind `ADR/0005` and the audit-completeness NFR forbid. `01 §3.6` *Attempt Transition* step 17 avoids both, by conflict-free insertion and by position.
* **The gate predicate must exclude the requesting order.** An amendment is issued by an order that already holds its key, so a predicate counting all holders without excluding its own subject refuses every amendment against itself. The transaction avoids the same trap by **partitioning** the resolved keys and re-offering only the ones the order does not already hold — so its own keys are never candidates for conflict. Ordering is check-then-mutate (`01 §3.6` *Attempt Transition* step 17); the release-first form that preceded it worked for self-collision and surrendered the order's key on a refusal (`DECISIONS.md` D-86). This trap is recorded because it has already been introduced twice.
* **Claims are never deleted, only released**, so the table grows with submit and amendment traffic and needs an index on `(order_id) WHERE released_at IS NULL` — the release path finds claims by order, and the unique index leads on `payer_tenant_id`.
* **An order that never reaches a terminal state holds its key forever.** `in_fulfillment` is deliberately expiry-exempt, so a wedged fulfilment blocks that key indefinitely. The design's answer is an operational SLA raised by the sibling gear, which is a process answer to a data problem and is the sharpest residual cost of this decision.
* **The cap is one, and this decision does not make it configurable.** D-83's first form added a slot column to express "at most N" so that raising `maxConcurrentActive` would admit concurrent in-flight orders. That overrode a PRD MUST without amendment, and is reversed: §6.1(f) and §6.1(g) are separate rules and only the former is configurable.

### Confirmation

**This gear has no implementation and no runtime tests**, so the checks below are labelled either
verifiable today or planned. Every behavioural check this decision needs is in the second group.

**Verifiable today, by reading the design set.** `01 §3.7` declares the partial UNIQUE on
`(payer_tenant_id, overlap_scope_key) WHERE released_at IS NULL` — a UNIQUE index expresses
*exactly one*, which is what PRD §6.1(g) fixes — declares the `(order_id) WHERE released_at IS
NULL` index the release path needs, states that claims are released and never deleted, and names
the terminal set of `§4.3` on whose transitions the release happens. That the release-before-
insert ordering is what lets an amendment re-claim its own key is likewise readable there. The
document-level invariant suite (`scripts/check-design-invariants.py`, run as `make design-check`
in CI) asserts that every `orders_inflight_overlap_claim.<column>` reference across the set — this
ADR included — names a real column, and that the index declarations name only that table's
columns.

**Planned, not yet written.** A concurrency check issuing two identical submits simultaneously
and asserting exactly one commits while the other returns `order-in-flight-for-key`; a check that
an amendment of an order holding its own key is admitted; a check that the claim is released on
every transition into the terminal set of `01 §4.3`, `rejected` included; and — the one this
decision most needs — a check that a partial-unique collision surfaces as the registered refusal
*with its audit entry and settled idempotency record committed*, rather than as an unmapped error
or as an aborted transaction. None of the four is recorded yet as a verification approach in
`01 §1.2`, which is their home, and the last cannot be written before `01 §3.6` specifies the
transaction-preserving acquisition.

## Pros and Cons of the Options

### A claim table with a partial unique index (chosen)

* Good, because the guarantee is a database constraint, so no code path — slice, migration or admin surface — can admit a second in-flight order on a key.
* Good, because it is evaluated inside the transition transaction, so two concurrent submits cannot both pass.
* Good, because the claim's mutable lifecycle (`released_at`) lives on a row that can carry it, which an append-only version-scoped line cannot.
* Bad, because a constraint violation must be deliberately mapped to a registered refusal *and* caught without aborting the transaction, or it surfaces as an unmapped error, or it takes the audit append and the settle down with it.
* Bad, because claims are released rather than deleted, so the table grows with traffic and needs its own index and retention thinking.
* Bad, because an order that never terminates holds its key indefinitely, and the only answer offered is an operational SLA.

### Application-level counting under the aggregate row lock

* Good, because it needs no extra table and no constraint.
* Good, because the refusal is naturally a registered reason with no error mapping.
* Bad, because the colliding orders are *different* aggregates, so the aggregate row lock does not serialise them and the count is still racy.
* Bad, because it moves the guarantee from something the database enforces to logic a refactor can weaken silently.

### `SERIALIZABLE` isolation for transitions touching an overlap key

* Good, because it is correct without any application reasoning about races.
* Bad, because it pays a global isolation cost for one local rule.
* Bad, because the collision surfaces as a serialisation failure with no registered reason, so the caller cannot tell what to change.

### A PostgreSQL advisory lock on the hashed key

* Good, because it serialises exactly the contending transactions and nothing else.
* Bad, because it is not durable: it is released on connection loss, so it cannot express a claim that must hold for the whole in-flight window of hours or days.
* Bad, because a hashed key collides silently, blocking unrelated purchases with no diagnosable reason.

### An upstream reservation in Subscriptions

* Good, because the key's owner would enforce its own cardinality, and route (a) of `Q-05` would resolve there too.
* Bad, because Subscriptions exposes no such capability and the ask is not registered.
* Bad, because order submission would then depend on a second system's write availability, which the fail-closed gate already makes costly enough.

## More Information

Superseded by nothing. `DECISIONS.md` D-26 records the original defect and the table; D-83 records
the reversal of its own first form and why route (b) does not resolve `Q-05`. This ADR exists
because the gear's only concurrency-correctness mechanism was recorded as a single table row with
no alternatives, which is precisely the shape a later author removes.

## Traceability

- **PRD**: [`../PRD.md`](../PRD.md) — §6.1(f) and §6.1(g) the overlap rules, §16 risks
- **DESIGN**: [`../design/01-foundation.md`](../design/01-foundation.md) §3.6 the transaction-preserving claim acquisition and release, §3.7 `orders_inflight_overlap_claim`; [`../design/03-gate-and-pin.md`](../design/03-gate-and-pin.md) §4.2 predicates 7 and 9

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-lifecycle-fr-order-submit` — the submit gate's ninth delta predicate is a pre-check over this constraint; the constraint, not the predicate, is what makes the requirement hold under concurrency
* `cpt-cf-bss-orders-lifecycle-nfr-order-snapshot-integrity` — two concurrent submits on one key would each pin a price for a purchase the other invalidates; refusing the second inside the transaction is what keeps a pin bound to an admissible order
* `cpt-cf-bss-orders-lifecycle-fr-order-amendment` — the release-before-insert ordering inside the transaction is what lets an amendment re-claim its own key rather than colliding with itself
* `cpt-cf-bss-orders-lifecycle-component-transition-engine` — the claim insert and release are engine writes inside the transition transaction; no slice touches the table
- **Decisions register**: [`../DECISIONS.md`](../DECISIONS.md) — D-26, D-83, Q-05
