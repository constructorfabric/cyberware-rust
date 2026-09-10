---
status: accepted
date: 2026-07-26
decision-makers: Constructor Fabric Steering Committee
---

# Write Path and Admission Protocol

**ID**: `cpt-cf-types-registry-adr-write-path-admission-protocol`

## Table of Contents

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Scope](#scope)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Actual mutations are always asynchronous](#actual-mutations-are-always-asynchronous)
  - [Request identity, content equality, and optimistic locking are distinct](#request-identity-content-equality-and-optimistic-locking-are-distinct)
  - [Dry run is a mode of the operation, not an operation of its own](#dry-run-is-a-mode-of-the-operation-not-an-operation-of-its-own)
  - [The public operation has one status and per-candidate results](#the-public-operation-has-one-status-and-per-candidate-results)
  - [Batches use dependency-aware partial admission](#batches-use-dependency-aware-partial-admission)
  - [Startup is caller-side reconciliation, not a global barrier](#startup-is-caller-side-reconciliation-not-a-global-barrier)
  - [Control-plane records are registry entities with built-in validators](#control-plane-records-are-registry-entities-with-built-in-validators)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Always synchronous](#always-synchronous)
  - [Deadline-based inline completion](#deadline-based-inline-completion)
  - [Synchronous no-op plus asynchronous mutation](#synchronous-no-op-plus-asynchronous-mutation)
  - [Always asynchronous with no synchronous no-op path](#always-asynchronous-with-no-synchronous-no-op-path)
  - [All-or-nothing batch](#all-or-nothing-batch)
  - [Unconstrained best effort](#unconstrained-best-effort)
  - [Dependency-aware partial admission](#dependency-aware-partial-admission)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

## Context and Problem Statement

Registration must support a bounded batch because one candidate may depend on another candidate that does not exist yet.

Admission also cannot be bounded by the size of the request. The authored document, resolution closure, and candidate count are capped, but admitting a content revision additionally revalidates every transitive dependent of the target. The dependent count is deliberately uncapped because platform base types are widely depended upon by design.

The work for revising such a type therefore follows from registry state, not payload size. Keeping it on an HTTP request would make the platform's most common contracts the most expensive to change. That is a P1 property and holds whatever else the write path acquires later.

P2 semantic hooks point the same way and are the second reason rather than the first: they may outlive an HTTP request, so a contract that was synchronous in P1 would have to be withdrawn when they arrive.

The first public contract must consequently support durable asynchronous execution.

Three forms of concurrency have to remain separate:

* a network retry must recover the original request rather than execute it twice;
* a caller must not overwrite a logical entity that changed after the caller read it;
* dependencies may change while expensive resolution and compatibility checks run outside a database transaction.

The common platform-gear case adds a startup constraint. A gear knows the definitions it desires, but Types Registry does not know the complete platform registration set and must not wait for one. The gear therefore needs a read/reconcile/write workflow and gates only its own readiness.

ADR-0002 also leaves Registry Source Plugin configuration as either a Managed Entity or platform configuration. That choice belongs here because it determines whether routing invariants pass through the ordinary admission protocol.

## Scope

This ADR decides:

* the acceptance and asynchronous operation response contract, and whether any acceptance may be synchronous;
* immutable request-key replay and content no-op semantics;
* per-entity optimistic preconditions;
* dependency-aware partial batch admission;
* caller-side startup reconciliation;
* whether control-plane records are registry entities and how P1 validates them.

Route paths, DTO field layout, operation retention, and table definitions belong to DESIGN. Revision identity and retention remain in ADR-0005 and ADR-0006, compatibility in ADR-0003, and ownership and authority in ADR-0009.

## Decision Drivers

* Admission work is not bounded by the request: revalidating the dependents of a widely used base type is unbounded by design.
* Enabling P2 semantic hooks must not break the client contract.
* A retry after a timeout must recover the same operation or completed response.
* Request identity must not change meaning when registry state changes later.
* A stale writer must not overwrite a newer revision.
* Expensive GTS checks must not hold a database transaction open.
* Independent valid candidates should not fail only because another branch in the same batch is invalid.
* Candidates with admission-order dependencies must be registrable in one batch without exposing an inconsistent intermediate state.
* Types Registry must not depend on a global startup census.
* P1 safety must not require P2 hooks.

## Considered Options

For execution:

* always synchronous;
* deadline-based inline completion with an operation fallback;
* synchronous no-op detection and always-asynchronous execution for actual mutations;
* always-asynchronous execution with no synchronous no-op path.

For concurrency:

* content equality alone;
* request idempotency alone;
* request idempotency plus an explicit entity resource-version precondition.

For batch failure:

* all-or-nothing;
* unconstrained best effort;
* dependency-aware partial admission in dependency order.

For startup:

* a global staged-registration barrier;
* caller-side read, reconcile, conditional registration, retry, and readiness gating.

## Decision Outcome

Chosen options, one per dimension: **always-asynchronous execution with no synchronous no-op path**, **request idempotency plus an explicit entity resource-version precondition**, **dependency-aware partial admission in dependency order**, and **caller-side read, reconcile, conditional registration, retry, and readiness gating**.

Together they provide:

* one acceptance shape that does not depend on load, timing, or whether the batch changes anything;
* a request key for replay and a resource version for stale-write protection;
* independent candidate progress without losing dependency ordering;
* caller-side readiness gating without putting Types Registry on the platform boot path.

None of these choices has to be withdrawn when P2 hooks make operation duration unbounded.

The sections below state each choice in full.

### Actual mutations are always asynchronous

A new registration request has exactly one successful acceptance result: `202 Accepted` with an operation UUID. The server never returns an admission result inline, whether because P1 validation happens to be fast or because the batch turns out to change nothing.

A synchronous `200 OK / unchanged` acceptance for an all-equal batch is not offered either — see *Sub-choices within the selected option*, below.

An all-equal batch is reachable: content identical to what was just read satisfies `match_resource_version(v)` without another writer. It is a redundant submission, not a race the server must resolve.

The preconditions rule out the raced form:

* `must_not_exist` yields `precondition_failed` once the entity exists;
* `match_resource_version(v)` fails once another write advances the version, even if that write makes the content equal.

`unchanged` is therefore a **guarantee** rather than an acceptance path: a redundant submission creates no revision and no `resource_version` increment. The operation's per-candidate result records that outcome.

The acceptance transaction stores the operation and its candidates and enqueues the operation UUID through the ToolKit transactional outbox. Request identity and workflow state are one record. A separate request record would exist only to hold a synchronous no-op receipt, and with no such path there is no request without an operation. The operation carries the scoped `Idempotency-Key`, its fingerprint, the plane, and the principal. The outbox owns leases, retry, duplicate delivery, and dead letters; the operation owns client-visible progress. A worker performs checks outside a long transaction and commits bounded admission units in short transactions.

This keeps one mutation contract when P2 hooks arrive. Hooks increase operation duration but do not introduce a new response shape — and now no successful acceptance has a second shape to begin with.

### Request identity, content equality, and optimistic locking are distinct

`Idempotency-Key` is mandatory for a registration batch and is scoped to the caller's plane, owning tenant, **and requesting principal**. Including the principal prevents one subject's key from returning another subject's Registry References and resource versions inside the same tenant.

The stored fingerprint covers at least:

* the canonical request body and operation kind;
* the ownership scope and dry-run mode;
* every candidate precondition;
* every per-candidate check waiver — in P1, ADR-0004's `force`.

The write path accepts `force` only when deployment configuration enables it; otherwise envelope validation refuses it. Mode, preconditions, and waivers are explicit fingerprint inputs even though they travel in the body. Omitting any of them could make a distinct submission replay the stored operation and never execute: a silent lost write.

A repeat with the same scoped key and fingerprint returns the immutable stored operation without re-evaluating current registry state:

* a non-terminal operation returns `202` and the same operation UUID;
* a terminal operation returns `200` and the stored terminal operation.

The same key with a different fingerprint is `409 Conflict`. A completed request never turns back into a new mutation because another request later changed its entities. A new reconciliation cycle uses a new key.

Content equality answers a different question: whether an admitted candidate creates a revision. Equal canonical authored content creates no revision and does not increment `resource_version`; the item status is `unchanged`. Effective resolved artifacts are not part of this comparison because dependency movement may change them without changing authored content.

Optimistic locking answers whether the caller's read is still current. Every candidate carries exactly one precondition:

* `must_not_exist` for an identifier that was absent during reconciliation;
* `match_resource_version(version)` for an existing entity.

The worker enforces the precondition in the commit transaction. A mismatch is a terminal per-item `precondition_failed`; Types Registry does not silently rebase the update. This token is the logical entity's monotonic `resource_version`, not the authored revision number, because lifecycle and other correctness-relevant changes must also invalidate a stale write.

Dependency freshness is internal concurrency control. Validation records the dependency revision vector. If the target precondition still holds but a dependency changes before commit, the worker revalidates within a bounded retry policy. This internal retry does not weaken the caller's target precondition.

**A compatibility baseline is a third thing again, and ADR-0004 keeps it out of this machinery.** For a minor-bearing candidate, the baseline is the preceding minor. It is neither the target nor a dependency: reference edges do not cross minor boundaries, so the baseline is absent from the dependency vector.

It must also remain absent from the `dependency` relation. Such an edge would make deletion safety refuse to delete `v1.0~` while `v1.1~` exists, although ADR-0008 permits that deletion and ADR-0004 relies on it.

Contiguity names the baseline from the candidate identifier; family state does not select it. There is therefore nothing to snapshot or for a concurrent admission to move. The commit transaction only tests, under the family lock, that `vM.(n-1)~` is still admitted as `ACTIVE` or `DELETED`, because concurrent delete-and-purge could remove it during validation.

Absence fails the candidate retryably, like a base not yet registered. It is not a caller-precondition failure: the caller declared no condition about the baseline identifier.

### Dry run is a mode of the operation, not an operation of its own

Every mutation kind accepts a dry-run request: it runs the complete check sequence and commits nothing — no logical entity, no revision, no current-pointer move, no `resource_version` advance, no lifecycle transition, no removal. Per-candidate statuses and diagnostics are what the real operation would have produced against the state observed during the run. A candidate that would have committed carries no revision and no resulting resource version, because nothing was written; that is the one respect in which the result differs from the one it predicts.

It is a mode rather than a separate validation operation, for the reason given under *Sub-choices within the selected option*, below.

The mode participates in the request fingerprint. Without that, a dry run and the real submission that follows it collapse under one `Idempotency-Key`: the second request replays the first's stored operation and never executes. That is a silent lost write, and it is the reason the fingerprint list above names the mode explicitly.

The acceptance shape does not change. A dry run returns `202` with an operation UUID and is polled like any other, which is not uniformity for its own sake: when P2 hooks exist a dry run **must** invoke every hook the real operation would, or it stops predicting admission precisely where the stakes are highest — and hook duration is unbounded. Giving the mode a synchronous contract in P1 would therefore mean withdrawing it in P2, which is the client-contract break this ADR exists to avoid.

A dry run is not a guarantee of admission and must not be presented as one. Its verdict is relative to the state it observed: a target's `resource_version` may advance, a dependency may admit a new revision, or the entity may be deleted before the real submission.

**Purge is outside this ADR, and its dry run is separate.** ADR-0013 defines purge as a synchronous platform-plane job with no operation, request key, per-candidate row, or outbox message.

The asynchronous-path reasons do not apply:

* P2 hooks do not run on purge, so local database work bounds its duration;
* a scan discovers its candidates, so there are no caller-named candidates to persist;
* it has no gear-facing contract that must remain stable across P2.

ADR-0013 does retain the important dry-run property: it is a mode of the same purge code, not a second implementation of its checks.

### The public operation has one status and per-candidate results

An operation has one status, and it carries **progress alone**: `pending`, `running`, or `completed`. `completed` asserts only that every candidate is terminal.

The outcome is not on the operation. It is the set of per-candidate statuses, keyed by exact GTS Identifier, and it stays there because an operation-level outcome would be a fold over those statuses — a stored copy of a derivable fact whose agreement with the items spans two tables and is expressible in no CHECK. `cpt-cf-types-registry-principle-derive-not-store` refuses that, as ADR-0008 does when it declines to record which member of a version family is newest.

The caller loses nothing it has to fetch. The operation resource carries its items, a batch is bounded at 100, and a caller asking *which* candidate failed — the common case — reads them regardless.

Each exact candidate GTS Identifier has one result with status `pending`, `running`, `succeeded`, `unchanged`, or `failed`. The term `unchanged` is preferred to `not_modified` and `already_registered`: it is intent-neutral and does not overload HTTP `304 Not Modified`. The status itself remains update-only, as follows.

`unchanged` is reachable **only for a registration update**, as a consequence of preconditions:

* deletion is a lifecycle transition with no redundant branch;
* creation declares `must_not_exist`, which fails once the entity exists;
* only a candidate carrying the `resource_version` it read can be proved redundant.

The database enforces the first two restrictions.

A terminal success always returns the Registry Reference. It also returns a resulting resource version for every committed result, including deletion. An `unchanged` result returns the version that did not move.

A **dry-run `succeeded`** is the exception: it predicts a commit whose version was never allocated, so it carries none. A dry-run `unchanged` carries the existing version it read.

The result does **not** return a revision number. No P1 operation accepts one, so it would be a handle attached to nothing; the caller's next write instead uses `resource_version` (ADR-0005, ADR-0006).

A failure returns a structured reason, and **that reason is the only carrier of what the check found**. ADR-0003 confines compatibility reporting to refusal, so successful items carry no verdict, mode, or classification. `operation_item` therefore needs no payload column beyond the existing one.

There is no separate public Admission Status vocabulary and no pending logical entity. An operation is the request-progress resource; an entity exists only after an admission unit commits.

**Operations are retained until nothing points at them, then bounded by a retention window.** Pinning is by **revision, not outcome**. An operation remains reachable from every revision it produced and therefore lives until those revisions are purged. This is also why revision tables do not duplicate the admitting principal.

The retention sweep may remove any unpinned operation:

| When unpinned | Operation class | Why |
|---|---|---|
| From completion | Dry run | It creates no revision. |
| From completion | No candidate succeeded | It created no revision. |
| From completion | Successful deletion | A lifecycle transition creates no content revision. |
| After purge | Its revisions were removed | Purge deletes revisions but leaves operation items. |

A status-based sweep would retain dry runs and successful deletions forever. The sweep must instead test revision references.

Removal releases the scoped request key, so a later replay executes afresh:

| Removed operation | Replay result |
|---|---|
| Dry run | Harmless because it has no effect. |
| Nothing admitted | Fails again, or succeeds because registry state changed. |
| Successful deletion | Fails its stale precondition: the entity is already `DELETED` at a later `resource_version`. |
| Revisions purged | Runs as an ordinary registration of a free identifier, subject to current authorization and its original precondition. |

In the last case, a precondition that permits creation can create a new logical entity; it does not restore the purged one. An original update carrying `match_resource_version` instead fails because the entity is absent. Purge releases the identifier for reuse and reserves nothing against it. This does not breach ADR-0013, which reserves removal of admitted content and identity to one operator-invoked act; replay of an unpinned operation is neither.

### Batches use dependency-aware partial admission

A batch is one durable operation on one plane, but it is not one all-or-nothing database transaction.

The candidate dependency graph is resolved with the submitted candidates as an overlay. A reference to another candidate never silently falls back to that identifier's previously committed revision. The graph is processed in topological order:

* one candidate is one admission unit;
* independent units that pass all checks commit even if another branch fails;
* a dependent whose selected in-batch dependency fails is itself failed, with a reason distinguishing it from a candidate that was evaluated and rejected.

This is called **dependency-aware partial admission**, not best effort. “Best effort” does not state dependency ordering or overlay resolution.

**Admission keeps the dependency graph acyclic.** It rejects cycles in the combined `$ref` and derivation graph because both edge kinds are inlined into the effective artifact. Derivation alone is acyclic because each edge shortens the identifier chain, and Instance conformance cannot close a cycle. The full graph, including conformance and the predecessor edge below, is then processed topologically, one candidate per admission unit. Traversals still deduplicate converging paths and bound work if stored data violates the invariant.

**The graph carries one implicit edge kind alongside authored edges.** Two minors of one major do not reference each other, yet `vM.n~` must follow `vM.(n-1)~` when both occur in a batch. An identifier-derived predecessor edge supplies that order. If the lower minor fails, it blocks the higher one rather than admitting it over a gap.

This edge provides determinism, not soundness. Without it, the higher minor fails retryably and succeeds on the next reconciliation cycle, as `cpt-cf-types-registry-fr-two-phase-init` already requires.

The edge:

* keeps the graph acyclic like every other edge kind, because minor numbers strictly increase along it;
* is never stored in the `dependency` relation, for the compatibility-baseline reason above.

The batch cannot mix ownership scopes. All candidates are tenant-owned by the one tenant context, or all are global under platform authority. The version-family row remains the single ownership authority, so concurrent first registration cannot make one family global and tenant-owned or assign it to two tenants.

### Startup is caller-side reconciliation, not a global barrier

On startup, a domain or platform gear:

1. batch-reads the current snapshots for its desired exact GTS Identifiers;
2. compares canonical authored content locally;
3. skips the POST when every desired definition is already current;
4. submits only missing or differing candidates with preconditions derived from the read;
5. polls the operation and gates its own readiness on the required per-candidate outcomes;
6. performs a fresh read and a new reconciliation cycle after a precondition failure or a dependency-not-yet-registered failure.

Types Registry becomes ready when its own storage and workers are ready. It does not know how many registrants exist, wait for a staged set, or coordinate readiness across gears.

The SDK provides this workflow as a convenience helper while retaining lower-level batch-get, submit, and get-operation methods. A helper invocation generates one key and reuses it across transport retries and polling. Crash-resumable callers may persist and supply the key.

### Control-plane records are registry entities with built-in validators

Registry Source Plugin configuration and, in P2, Validation Hook declarations are global Managed registered Instances of platform-defined GTS Types. They pass through the same revisions, concurrency, operation, and audit protocol.

Types Registry provides a closed in-process validator for these platform-defined control-plane types. It is not registered or extensible and is distinct from P2 semantic hooks. It enforces Source Claim non-overlap, claim pattern grammar, retired-claim reservations, and the prohibition on tenant-scoped control-plane instances. Their base schemas are trusted platform seeds and do not depend on user definitions.

### Consequences

* Real registration work always costs at least acceptance plus operation polling; the SDK hides that loop for callers that want a terminal result.
* Every accepted POST costs an operation, its candidate rows, an outbox message, and a poll. The startup helper avoids the POST entirely, so no correct path pays this.
* Request identity needs no storage of its own: the operation is the receipt, so replay and conflict semantics cost one unique constraint rather than a second table and a second insert per write.
* The content-equality rule is implemented once, in the worker, instead of also as a whole-batch predicate under a row lock in the request handler.
* Clients distinguish request retry from a new reconciliation attempt. Reusing a completed key does not ask the server to compare against today's state.
* Every update candidate carries an explicit resource version, and every create candidate explicitly requires absence.
* Batch outcomes are deterministic with respect to dependency units, but a mixed success/failure result is valid.
* Types Registry leaves the platform startup critical path; each registrant owns its retry and readiness.
* The worker must be safe under outbox redelivery and lease expiry.

### Confirmation

This decision is confirmed when:

* every accepted request returns `202` with an operation UUID without waiting for admission, and no successful acceptance has any other shape;
* an all-equal request is accepted as an ordinary operation whose candidates all terminate `unchanged`, and no storage path produces a request record without an operation;
* a matching key replay returns the same operation even after another request changes current state;
* the same scoped key with another fingerprint is rejected;
* two callers on different planes may use the same key value without collision, and so may two principals in one tenant;
* an update whose target resource version changed fails with `precondition_failed` and creates no revision;
* a dependency-only race is revalidated without bypassing the target precondition;
* a minor whose predecessor is absent at commit fails retryably rather than as a caller-precondition failure, and no predecessor relationship appears as a row in the `dependency` relation — deleting `v1.0~` while `v1.1~` exists still succeeds;
* two minors of one major in one batch are admitted in ascending order, and the higher one is blocked when the lower one fails;
* equal authored content creates no revision and does not advance resource version, and `unchanged` is reachable only for an update: a deletion and a `must_not_exist` create are both refused that status by the stored-state constraint as well as by the worker;
* a dry-run `succeeded` carries no resulting resource version while a dry-run `unchanged` carries the existing one, and both carry the derived Registry Reference;
* an operation whose revisions purge removed becomes sweepable; after the sweep, replay of an original `must_not_exist` create may register a new logical entity under the free identifier, while replay of a `match_resource_version` update fails because the entity is absent — neither restores the purged entity;
* independent valid candidates commit when another dependency branch fails;
* a failed in-batch dependency blocks its dependants;
* the operation status takes only `pending`, `running`, or `completed`, and no operation-level field states whether the batch succeeded — a caller establishes that from the candidate statuses, and a mixed batch is `completed` exactly as a wholly successful one is;
* an operation completed after its worker gave up is indistinguishable at the operation level from one whose candidates were rejected on their merits, and distinguishable per candidate, which is the level at which the difference is real;
* cycles in the combined `$ref` and derivation graph are refused, both within one batch and by a revision that would close a cycle across operations;
* concurrent first-family admission leaves exactly one owner;
* the retention sweep removes a terminal operation exactly when no revision reaches any of its items, which is exercised on a successful deletion and a successful dry run — both of which carry `succeeded` items and no revision — as well as on an operation in which nothing succeeded;
* a gear with current definitions performs only the batch read and reports `UpToDate`;
* a gear with stale definitions submits a conditional batch, polls it, and gates only its own readiness;
* Types Registry reaches ready state before any domain gear registers definitions;
* tenant-scoped control-plane registration is rejected and Source Claim invariants are enforced without P2 hooks;
* a dry run of each mutation kind reports the same per-candidate statuses and diagnostics as the real operation while leaving every entity, revision, current pointer, resource version, and lifecycle status untouched, and returns no revision and no resulting resource version for a candidate that would have committed;
* a dry run and a real submission carrying the same scoped key are treated as different requests, so the real submission executes rather than replaying the dry run;

## Pros and Cons of the Options

### Always synchronous

* Good, because P1 callers receive a terminal result in one request.
* Bad, because P2 hooks and wide dependent revalidation may outlive request deadlines.
* Bad, because a later switch to operations would break every client.

### Deadline-based inline completion

* Good, because short mutations sometimes finish in one round trip.
* Bad, because response shape depends on load and timing rather than semantics.
* Bad, because it couples the HTTP request lifetime to worker execution and complicates duplicate execution.

### Synchronous no-op plus asynchronous mutation

* Good, because response shape depends on semantics rather than on load or timing.
* Good, because a redundant raw POST terminates in one round trip.
* Bad, because strict replay of the no-op requires a separate durable receipt, and therefore a second table holding the optional half of a one-to-one relation.
* Bad, because the whole-batch equality predicate duplicates, under a row lock in the request handler, a rule the worker already applies per candidate.
* Bad, because the state it optimizes is reached only by a caller that reconciled and submitted anyway, and such a caller would not have sent the request.

### Always asynchronous with no synchronous no-op path

* Good, because no successful acceptance has a second shape, so the response depends on nothing at all.
* Good, because request identity and workflow state collapse into one record, sparing a table, a foreign key, an insert per write, and a `disposition` discriminator.
* Good, because the content-equality rule has exactly one implementation and no row-locked snapshot read remains on the acceptance path.
* Bad, because a redundant submission now costs an operation, its rows, an outbox message, and a poll.
* Bad, because `unchanged` appears in both vocabularies while being unreachable for a correct caller, which reads as dead vocabulary until explained.

### All-or-nothing batch

* Good, because it has a simple outcome.
* Bad, because an unrelated invalid candidate prevents valid independent registrations.
* Bad, because expensive validation encourages an excessively large transaction or repeated work.

### Unconstrained best effort

* Good, because independent candidates may succeed.
* Bad, because the term does not define dependency fallback or ordering.

### Dependency-aware partial admission

* Good, because it preserves independent progress, orders dependencies, and blocks downstream candidates when a selected dependency fails.
* Bad, because clients must inspect per-GTS-ID results and handle partial success.

### Sub-choices within the selected option

Alternatives considered while shaping the option above, recorded here rather than in *Decision Outcome*, which states what was chosen.

A synchronous `200 OK / unchanged` acceptance for an all-equal batch is not offered either, and the reason is not economy of response shapes. The state such a path would optimize is reached only by a caller that read current state, compared it, found equality, and submitted anyway — which is a caller defect rather than a workflow, since a caller that reconciled sends no POST at all. The optimization belongs there, where it costs nothing, and §Startup places it there.

It is a mode rather than a separate validation operation for one reason. A separate operation is a second implementation of the ordered check sequence, and the two drift; a drifted check that passes before deployment and fails at admission is the exact failure a pre-deployment gate exists to prevent. As a mode it is the same code path with the commit suppressed, so drift is not merely discouraged but unrepresentable. It is consequently orthogonal to `kind` rather than three further values in it.

**Neither vocabulary carries cancellation, expiry, or a blocked candidate.**

* **Cancellation:** no P1 requirement, actor, or use case asks to cancel a mutation in flight. P2 hooks may make this worth a separate decision later.
* **Expiry:** outbox redelivery and idempotent commits recover a dead worker. After retries are exhausted, terminal `succeeded` and `unchanged` items retain their outcomes; remaining non-terminal items become `failed` with structured reasons. A timeout is completed the same way.
* **Blocked candidate:** a candidate that an in-batch dependency prevents from being evaluated is `failed` with a distinct reason.

Per-candidate outcomes keep “some candidates were rejected” distinct from “the worker gave up,” which a merged operation outcome would obscure.

The governing rule is: **status distinguishes effects; reason distinguishes causes**. `succeeded` changed the entity, through a revision or a lifecycle transition. `unchanged` proved the submission redundant and changed nothing. All failed ways of producing nothing differ only by reason.

## More Information

* [Google AIP-151 Long-running operations](https://google.aip.dev/151) describes a durable operation resource for work that may outlive a request.
* [Stripe idempotent requests](https://docs.stripe.com/api/idempotent_requests) treats the key as request identity and replays the original result.
* [Kubernetes resource versions](https://kubernetes.io/docs/reference/using-api/api-concepts/#resource-versions) illustrate an opaque concurrency token distinct from resource content.

## Traceability

- **PRD**: [../PRD.md](../PRD.md)
- **DESIGN**: [../DESIGN.md](../DESIGN.md)
- **Database reference schema**: [../database.sql](../database.sql)
- **ADR-0002**: [0002-cpt-cf-types-registry-adr-external-source-live-delegation.md](./0002-cpt-cf-types-registry-adr-external-source-live-delegation.md)
- **ADR-0003**: [0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md](./0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md)
- **ADR-0005**: [0005-cpt-cf-types-registry-adr-type-schema-revisions.md](./0005-cpt-cf-types-registry-adr-type-schema-revisions.md)
- **ADR-0006**: [0006-cpt-cf-types-registry-adr-registered-instance-revisions.md](./0006-cpt-cf-types-registry-adr-registered-instance-revisions.md)
- **ADR-0009**: [0009-cpt-cf-types-registry-adr-tenant-ownership-visibility-authority.md](./0009-cpt-cf-types-registry-adr-tenant-ownership-visibility-authority.md)
- **ADR-0011**: [0011-cpt-cf-types-registry-adr-managed-external-boundary.md](./0011-cpt-cf-types-registry-adr-managed-external-boundary.md)

This decision directly addresses:

* `cpt-cf-types-registry-fr-register-schemas`, `cpt-cf-types-registry-fr-register-instances`;
* `cpt-cf-types-registry-fr-dry-run`;
* `cpt-cf-types-registry-fr-two-phase-init`;
* `cpt-cf-types-registry-fr-registry-source-routing`;
* `cpt-cf-types-registry-fr-validation-hooks`;
* `cpt-cf-types-registry-interface-sdk`, `cpt-cf-types-registry-interface-rest`.
