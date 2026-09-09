---
status: accepted
date: 2026-07-27
decision-makers: Constructor Fabric Steering Committee
---

# Platform-Level Purge of Deleted Registry State

**ID**: `cpt-cf-types-registry-adr-platform-purge-of-deleted-entities`

## Table of Contents

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Scope](#scope)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [One operation](#one-operation)
  - [Everything that records the identity](#everything-that-records-the-identity)
  - [Optimistic tokens do not survive purge](#optimistic-tokens-do-not-survive-purge)
  - [What guards purge](#what-guards-purge)
  - [Preconditions and shape](#preconditions-and-shape)
  - [The exception to identifier non-rebinding](#the-exception-to-identifier-non-rebinding)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [A deployment mode in which ordinary deletion is physical and identifiers are reusable](#a-deployment-mode-in-which-ordinary-deletion-is-physical-and-identifiers-are-reusable)
  - [A retention period after which deleted entities are physically removed](#a-retention-period-after-which-deleted-entities-are-physically-removed)
  - [An explicit platform-level purge, split into content removal and identity removal](#an-explicit-platform-level-purge-split-into-content-removal-and-identity-removal)
  - [One explicit platform-level purge](#one-explicit-platform-level-purge)
- [More Information](#more-information)
  - [Industry Practice](#industry-practice)
- [Traceability](#traceability)

<!-- /toc -->

## Context and Problem Statement

Deletion in Types Registry is logical and terminal. `DELETED` is final in P1, admitted content revisions are retained, no retention policy ever removes them, and ADR-0001 reserves the GTS Identifier permanently so it can never be rebound to a new logical entity.

Those properties are correct for production. They also make an ordinary development mistake unrecoverable. A developer who registers a schema under a mistyped identifier, or registers one shape and then needs an incompatible one, has burned that identifier for the lifetime of the deployment. Iterating on a schema before it has any consumers is exactly the case the production guarantees were not written for.

The obvious fix — a deployment mode in which deletion is physical and identifiers are reusable — was considered and rejected during design discussion, for a reason worth recording: it makes the development environment stop being a rehearsal of production. Reverse resolution of a deleted entity's Registry Reference, rejection of re-registration under a deleted identifier, and tombstone retention are precisely the behaviours most likely to harbour bugs, and a mode that changes ordinary deletion hides all three.

Recovering a burned identifier is the whole of the problem.

## Scope

This ADR decides:

* whether physical removal of registry state exists, and through what mechanism;
* what it removes, and what has to be removed with it for an identifier to be genuinely free;
* what guards it, given that no check can establish its safety;
* the delivery shape, authority, and audit of the operation;
* the single exception this creates to the identifier non-rebinding guarantee.

This ADR does not decide deletion preconditions or lifecycle transitions (ADR-0008, PRD `cpt-cf-types-registry-fr-lifecycle`), the write-path contract the operation uses (ADR-0012), the retired Source Claim reservations of ADR-0011, or the platform data-classification policy that decides which content may be registered under the retention terms recorded here.

## Decision Drivers

* A development environment must exercise the same deletion semantics as production, or it stops being evidence about production.
* Releasing an identifier for reuse is a data-corruption primitive, not a storage optimization: deterministic derivation gives the reused identifier the same Registry Reference, so any domain row still holding it silently rebinds to an unrelated entity.
* No check available to Types Registry can establish that no domain row holds a given Registry Reference. Safety cannot be proven, only bounded.
* Physical removal must never be automatic. A background policy that quietly discards registry state is the failure mode retention rules exist to prevent.
* A mechanism that only partly achieves what its name promises is worse than none, because it invites reliance it cannot support.

## Considered Options

* A deployment mode in which ordinary deletion is physical and identifiers are reusable.
* A retention period after which deleted entities are physically removed.
* An explicit platform-level purge operation, separated into content removal and identity removal.
* One explicit platform-level purge operation that removes both.

## Decision Outcome

Chosen option: **one explicit, operator-invoked platform-level purge that physically removes an entity's records and releases its GTS Identifier.**

Ordinary deletion is unchanged everywhere. Every deployment, including development, exercises the same logical deletion, the same tombstone retention, and the same identifier reservation. What differs between deployments is whether one privileged operation is available at all, not how deletion behaves.

### One operation

Purge removes the records of a `DELETED` entity and releases its identifier for registration as a new logical entity. It is the operation development needs, and the one that can corrupt data.

**A content purge is not production-safe.** P1 deletion sees only registry state. `cpt-cf-types-registry-fr-lifecycle` therefore permits deleting a Type Schema while live domain data still conforms to it; P2 owning-gear Validation Hooks may close that limitation later.

The gear holding that data must retire, migrate, export, or re-type it under `cpt-cf-types-registry-principle-contract-not-object`. A tombstone is insufficient: availability says the contract is gone, but handling an object requires its authored document, resolved effective schema, and effective traits.

Deleting that payload would silently strand the very data logical deletion leaves behind, because the registry cannot see it. An exact read of a deleted entity therefore still serves its content groups (DESIGN §3.3, *Read results*).

The invariant reaches the **current** revision at deletion — the one every surviving object validates against, since ADR-0003 makes each revision accept everything its predecessors accepted. It does not by itself justify retaining the earlier ones; what those are retained for is DESIGN open question D4.

The tombstone, the mapping, and any other durable record of the identity are removed in **one transaction**. A partial removal that leaves a mapping behind would let the identifier be re-registered while a stale record still points at the old entity.

The observable states are:

| Stage | Old Registry Reference |
|---|---|
| Before purge | Resolves to a deleted, unavailable entity, distinguishing a retired contract from an unknown one. |
| After purge | Resolves to nothing, like a reference never issued. |
| After re-registration | Resolves to the new logical entity under the reused identifier. |

Deterministic derivation makes identifier and reference the same fact: the reference cannot remain retired while the identifier is reusable. This unavoidable rebinding hazard is why purge is disabled by default.

### Everything that records the identity

"Any other durable record of the identity" is not rhetorical, and the enumeration matters because a survivor does not merely leave litter — it silently changes what re-registration means. Beyond the entity row and the Registry Reference mapping, two records name the identity and are easy to overlook.

The **version-family record** binds the family key to an ownership scope. If it outlives its last member, the released name stays bound to the previous owner and, under ADR-0004's kind-exclusive family key, to the previous entity kind. A new registrant would be refused by a family that has nobody left to belong to. Purge therefore removes a family record once it is empty.

The **operation history** names the identifier on every candidate item that named it, and it is deliberately **not** in this enumeration: purge leaves those rows untouched. An earlier form of this decision deleted them, on the reasoning that a re-registration would otherwise leave a history in which one identifier string spans two logical entities. That reasoning was wrong twice over.

Deleting operation items would give up two properties:

* ADR-0012 promises immutable replay with a result for every original candidate. Removing an item retracts that result, including from a mixed batch pinned by another candidate's surviving revision.
* It cannot be race-free. Acceptance reads no registry state and takes no `version_family` lock, so purge could remove a newly accepted candidate before its worker runs, or let it execute against re-registration. Locking acceptance would add registry locking to a deliberately read-free path merely to protect a receipt.

It would buy nothing necessary. An operation is a **request receipt**, reachable only by operation ID or by the submitting principal's scoped idempotency key. No identifier-keyed operation query can splice two incarnations into one entity history; revisions are the entity history, and purge removes them.

The row contains neither Registry Reference nor entity kind. Its strongest handle is `resource_version`, whose limits §*Optimistic tokens do not survive purge* states. A retained receipt may name an absent or reused identifier, but remains a true record of the past request. Purge also unpins operations whose only revisions it removed, allowing the retention sweep to clear most within its window.

### Optimistic tokens do not survive purge

A `resource_version` is a per-entity counter that starts at 1 and advances with each write. A re-registered identifier is a **new** logical entity, so its counter starts at 1 again, and nothing durable distinguishes the two incarnations — which is the whole point of releasing the identifier. Two consequences follow and are **accepted as part of this operation's hazard rather than defended against**:

* a token a caller holds across a purge normally fails, because the new incarnation's counter is behind the old one — but once the new entity has taken as many writes as the old one had, the numbers **collide** and a stale precondition can be satisfied by an entity the caller never read;
* the same is true of a write accepted before the purge and executed after it, in the narrow window where the re-registration has already caught up.

This is strictly weaker than the hazard §*One operation* already states, and it is stated here so that no reader takes optimistic locking to be the thing that survives. If a stored Registry Reference silently rebinds to a different logical entity — which it does, because deterministic derivation makes identifier and reference the same fact — then a numeric token over that same identifier rebinding too adds no new class of failure. Both are why the operation is disabled by default and documented as unsafe wherever domain data may hold the reference.

### What guards purge

Nothing can prove purge is safe. Types Registry cannot see domain rows, so it cannot establish that no gear still holds the Registry Reference. A grace period proves nothing either — a domain row can be older than any interval.

The guard is therefore deployment policy: whether the operation is available at all. It is disabled by default and enabled deliberately, which in practice means enabled in development and scratch environments and left off in production except for a specific, planned migration.

This is a narrower divergence than the rejected deployment mode, and the difference is the point. Only the availability of one maintenance operation varies. Every ordinary code path — admission, resolution, deletion, reverse resolution of a deleted reference — is identical in every environment, so production scenarios remain testable on a development stand.

Where purge is enabled, it is documented as unsafe whenever domain data may hold the reference. In a development environment nothing holds it, which is what makes the operation reasonable there and not elsewhere.

### Preconditions and shape

Purge requires the entity to be `DELETED`, and re-evaluates the deletion preconditions at execution time: no registered dependent may exist. Under ADR-0011 every dependent is a Managed Entity, so that re-evaluation reads managed storage alone and reaches no plugin.

**A minor may be purged only from the top of its major.** Where ADR-0004's minors are in use, the minors of one major that a purge releases **MUST** form a suffix of that major's admitted sequence: releasing `v1.1~` while `v1.2~` is still admitted is refused, with the higher minors listed, exactly as an exact identifier still pinned by an Instance is refused with those Instances listed below.

**Purge follows the common writer order; the protocol is part of this decision.** It **MUST** advance `types_registry__coordination_state.entity_write_order` as the transaction's first statement, thereby claiming the row, then lock affected `version_family` rows in canonical order and hold them through commit. Entity rows follow, and the federation routing generation advances last. Every entity-state writer uses this order, which serializes admission and purge and avoids deadlocks.

Without serialization, either race could create a gap:

* purge deems `v1.0~` eligible, admission confirms it as the baseline for `v1.1~`, then purge removes it before admission commits;
* purge sees no successor to `v1.1~`, admission commits `v1.2~`, then purge removes `v1.1~`.

Deletion preconditions do not catch these races because ADR-0004 stores no dependency edge between minors. The common writer order and family locks close the gap.

Unlike other purge preconditions, the suffix rule protects a guarantee rather than a foreign key. ADR-0004 requires contiguous minors and counts a `DELETED` predecessor as present so its number cannot be reoccupied.

Otherwise, re-registering purged `v1.1~` would check it against `v1.0~` while existing `v1.2~` leaves `v1.1~ ≤ v1.2~` unestablished. That silently breaks the promised safe-upgrade chain of a stable, unforced major.

Purge cannot reserve the number because leaving no identity record is its purpose. The restriction must therefore govern what may be released. Together the rules keep admitted minors at `{0..k}`, growing and shrinking only at the end.

This is the one purge hazard Types Registry **can** decide, so it is a check rather than a documented risk. Whether a domain row still holds the Registry Reference is unknowable and remains with deployment policy and operator judgement, as §*What guards purge* explains.

A pattern selecting a whole major or family satisfies the suffix rule only when every higher selected minor is also eligible and released. If an `ACTIVE` higher minor is skipped, a lower minor cannot be released. The rule therefore constrains the actual release set, not merely the pattern.

To purge a middle minor, an operator must first retire the tail above it. That is the necessary cost of a sequence consumers were promised they could walk.

**Purge is synchronous and creates no operation.** It completes within the request and returns its report directly. The reasons for ADR-0012's asynchronous registration and deletion path are absent:

* P2 Validation Hooks do not apply, so only local database work bounds duration;
* the work is a managed-storage scan and delete, with no GTS resolution, compatibility check, or plugin call;
* it is a disabled-by-default, operator-invoked platform job, not a gear-facing contract that must survive P2.

Three absences follow:

* **No `Idempotency-Key` or replay record.** Re-running the pattern reports released identifiers as unmatched, making the job naturally repeatable.
* **No per-candidate row.** A scan expands the pattern, and per-identifier outcomes live in the response.
* **No registry operation history to erase.** Purge writes no registry operation row of its own and leaves prior operation items intact for the reasons in §*Everything that records the identity*. Its external job audit is separate.

A candidate still in flight therefore needs no special treatment. An operation accepted before purge keeps its rows when worked afterwards. A registration carrying `must_not_exist` may admit a new logical entity under the released identifier; an update carrying `match_resource_version` fails because the entity is absent; a deletion likewise fails as absent. None is a corrupted record, and purge need not serialize against acceptance.

The audit trail is therefore the job's own record rather than a registry row. What such a record must contain, who may read it, and how long it is kept is PRD open question 2, which covers registry mutations generally and is not settled by this decision.

It is delivered as a platform maintenance job rather than as tenant-facing API surface. That follows from what it is: a non-tenant-scoped platform operation authenticated on the platform plane with `PlatformSecurityContext` rather than a propagated tenant context, batch-shaped, and potentially wide in scope. A job also gives the operation a natural place for the property that matters most in practice — a **dry run** that reports exactly which identifiers would be released before anything is removed, broken down by owner, since one pattern can cross tenant boundaries.

The job takes a **GTS pattern**, making it usable while preserving referential integrity. A registered Instance identifier begins with its Type Schema identifier. A prefix pattern selecting a schema therefore also selects every Instance that could pin its revisions — a property of chained identifiers.

The job removes matched Instances before matched Type Schemas, so pins do not obstruct it. An exact identifier without a wildcard selects only itself; the job then **MUST** verify that no Instance pins the target and refuse with those Instances listed rather than fail on a constraint.

A pattern selects candidates; it does not waive preconditions. The job reports how many entities matched, how many were eligible, why each of the rest was skipped, and — for the dry run — the owner of every identifier it would release. All of that is in the response, computed while the entities are still in hand, which is what a synchronous shape buys: a dry run removes nothing, so the owner of a matched entity is a read away rather than something that has to be recorded before the entity disappears.

This dry run belongs to the purge job, not ADR-0012's general write path. It keeps the essential property: a **mode of the same code** that runs identical checks and stops before removal, preventing drift from real purge.

It needs no request-identity rule because purge stores no replayable request. It also guarantees nothing about a later invocation; eligibility may change in between.

Purging a Registry Source Plugin also removes its retired Source Claims. The transaction removes claim rows before their referenced plugin revisions, then removes matched entities and advances the routing generation last. This satisfies `ON DELETE RESTRICT` and makes the purge visible in one routing generation.

Purge never runs on a schedule, on a timer, or as a consequence of any retention rule. Every execution is an explicit act with an operator behind it.

### The exception to identifier non-rebinding

ADR-0001 guarantees that a logically deleted GTS Identifier cannot be rebound to a new logical entity. Purge is the single, named exception to that guarantee, and it is the reason the guarantee is stated as a property of ordinary operation rather than of the storage layer.

The same exception releases ADR-0011's retired Source Claim reservations. A Registry Source Plugin is a registered Instance, so purging it removes its identity and reservations; the claimed space becomes registrable again.

Excluding reservations would treat the same hazard differently: releasing a claim space can rebind a persisted reference just like releasing an identifier. ADR-0011 has no runtime takeover, so purge is the only in-product reuse path. It releases the space to the next registrant, including a managed one.

The narrower alternative is an out-of-product migration that retargets claim rows to a named successor while keeping the space continuously reserved.

### Consequences

* Development iteration is delete, purge, re-register, with production-identical semantics at every step. That is the trade this decision makes deliberately. The job may batch the first two over a pattern — deleting in dependency order and then purging — which is sequencing rather than new semantics: each step keeps its own preconditions and produces its own operation record, so the development stand still rehearses production.
* Retained content is unremovable in a production deployment **while purge stays disabled there**, because the one operation that removes it also releases the identifier. That is the default and the expected steady state, not an absolute: §*What guards purge* leaves enabling it a decision an operator can make for a specific, planned migration, and one such migration is named by `cpt-cf-types-registry-fr-registration-policy` — repairing an entity admitted as tenant-owned in a GTS Identifier Region a deployment opened by mistake, which ownership immutability leaves no other route to. The rebinding hazard of §*One operation* applies to that migration in full and is what keeps it exceptional. Whether a given class of content may be held on the default terms is a platform data-classification question rather than a registry one: Types Registry stores what it admits and applies no content policy of its own.
* Retention of the admitted revisions of ADR-0005 and ADR-0006 is unbounded by policy and bounded only by explicit purge.
* The identifier non-rebinding guarantee of ADR-0001 has exactly one exception, and this operation is it.
* A deployment must expose whether purge is enabled, so that an operator can tell before invoking it.
* Enabling purge in production is a decision an operator can make. The documentation must state plainly that it can silently rebind persisted domain references, because no runtime check will say so.
* Referential integrity between a registered Instance revision and the Type Schema revision that validated it survives purge without weakening, because the pattern that selects a schema also selects its Instances. Had the job taken a list of exact identifiers instead, those foreign keys would have had to be dropped.

### Confirmation

This decision is confirmed when:

* ordinary deletion behaves identically with purge enabled and disabled, and a development deployment reproduces production reverse resolution, re-registration rejection, and tombstone behaviour;
* a deleted entity is absent from discovery, search, and query assistance, and is returned by an exact read — by GTS Identifier or by Registry Reference alike — marked deleted and unavailable, so that a gear holding a stored reference can tell *deleted* from *never existed*;
* purge removes the entity record, its revisions, and the forward and reverse mapping in one transaction, after which the old Registry Reference resolves to nothing and is indistinguishable from an unissued reference;
* purge removes the version-family record once its last member is gone, and a subsequent registration of the released identifier under a different owner, or of the other entity kind, succeeds;
* purging a Registry Source Plugin removes its retired Source Claims, after which a Managed Entity can be registered in the space they reserved, while deleting that plugin without purging it leaves the reservations in force;
* purge leaves every operation item naming a released identifier in place, so a same-key replay returns every original candidate result while the operation remains retained; once the now-unpinned operation is swept, ADR-0012's fresh-execution rules apply according to the original precondition;
* a `resource_version` held across purge and re-registration is exercised deliberately, on every backend and for an update and a deletion alike: while the new incarnation is behind the old counter the stale token fails `precondition_failed`, and once it has caught up the token is **accepted** — the test asserts the documented behaviour of §*Optimistic tokens do not survive purge* rather than a guarantee it does not make, and the same case is run for a write accepted before the purge and executed after it;
* a purge returns its report in the response and creates no operation, no operation item, and no outbox message; re-running it over the same pattern reports the already-released identifiers as unmatched rather than failing, so repeatability needs no stored request identity;
* registration of a new logical entity under a purged identifier succeeds and resolves under the same Registry Reference as the purged one;
* purge rejects an entity that is not `DELETED`, and one that still has a registered dependent, with the precondition re-evaluated from managed storage while every plugin is unreachable;
* purge of `v1.1~` is rejected while `v1.2~` is admitted, with the higher minors listed, and succeeds once they are released;
* a purge concurrent with an operation already accepted for a matched identifier leaves that operation intact and reachable by its key, and the operation reaches `completed` with every candidate terminal: a registration with `must_not_exist` may admit a new logical entity, while an update with `match_resource_version` and a deletion both fail as absent;
* a purge concurrent with an admission into the same family produces no gap in either direction — neither a minor admitted over a purged predecessor nor a predecessor released under a concurrently admitted successor — which is exercised by driving both against the same family row; a pattern releasing every minor of a major is accepted; and after purging a whole major, re-registering `v1.0~` and then `v1.1~` re-establishes the sequence from scratch — proving the released numbers are reoccupied only in order;
* a pattern that selects a Type Schema also selects every Instance conforming to it, Instances are removed before schemas, and no foreign key obstructs the job; an exact identifier still pinned by an Instance is refused with those Instances listed;
* the job reports matched, eligible, and skipped counts with a reason per skipped entity, and a dry run reports the identifiers that would be released, broken down by owner, and removes nothing;
* purge is unavailable, and reported as unavailable, in a deployment where it is not enabled;
* no scheduled task, retention sweep, or background process removes **admitted content or identity** — a revision, an entity, a tombstone, a version family, or a Source Claim reservation — in any deployment. The one scheduled removal the platform does operate, the operation-retention sweep of DESIGN §3.2, is bounded to operations that no revision points at, so it releases no identifier and can rebind nothing.

## Pros and Cons of the Options

### A deployment mode in which ordinary deletion is physical and identifiers are reusable

* Good, because development iteration is a single operation with no extra step.
* Good, because it needs no new API surface at all.
* Bad, because the development environment stops rehearsing production. Reverse resolution of a deleted reference, rejection of re-registration, and tombstone retention are never exercised — and those are the behaviours most likely to be wrong.
* Bad, because the divergence is broad and implicit: every deletion behaves differently, rather than one operation being additionally available.

### A retention period after which deleted entities are physically removed

* Good, because it requires no operator action and bounds storage growth automatically.
* Bad, because a background process that releases identifiers would rebind persisted domain references with no human in the loop and no event anyone observes.
* Bad, because elapsed time is unrelated to whether a reference is still held; the interval would be a guess presented as a guarantee.
* Bad, because it makes registry state disappear silently, which is the outcome retention rules exist to prevent.

### An explicit platform-level purge, split into content removal and identity removal

* Good, because the half that removes content without releasing the identifier looks production-safe, so only the other half would need gating.
* Bad, because it is not safe: it would require the entity to be `DELETED`, which is precisely the state in which the owning gear still needs the contract to retire domain data that conforms to it (§One operation). It removes what its one remaining caller is there to read.
* Bad, because it doubles the operation surface, the guards, and the vocabulary for one act.

### One explicit platform-level purge

* Good, because ordinary deletion is identical in every deployment, so a development stand remains evidence about production.
* Good, because every removal has an operator, an audit record, and an available dry run.
* Good, because one act has one risk profile and one guard, and nothing has to explain which half an operator wants.
* Bad, because development iteration costs an extra step.
* Bad, because its safety rests on deployment policy and operator judgement rather than on anything the registry can verify.
* Bad, because a production deployment has no way to remove retained content at all, so what may be registered has to be governed before admission rather than corrected after it.

### Sub-choices within the selected option

Alternatives considered while shaping the option above, recorded here rather than in *Decision Outcome*, which states what was chosen.

The alternative is to split it in two, adding a content purge that removes retained revisions while keeping the identity tombstone, on the ground that removing content without releasing the identifier would be production-safe. It would not be, and the paragraph below says why. With that the argument for a split goes: what remains is one act with one risk profile and one guard.

A non-reusable incarnation identifier carried by each entity and write precondition would prevent numeric token collision. It is **declined** for two reasons:

* To be non-reusable, it must survive purge. That is precisely the durable record of a released identity that §*Everything that records the identity* requires purge to remove; a monotonic per-identifier counter means re-registration is not a fresh start.
* It protects callers holding a version token while leaving the wider Registry Reference rebinding hazard open.

Optimistic locking therefore remains exact **within one incarnation**, the scope of every other guarantee in this ADR.

## More Information

### Industry Practice

* [Confluent Schema Registry](https://docs.confluent.io/platform/current/schema-registry/schema-deletion-guidelines.html) separates a soft delete, which keeps the version recoverable and its identifier reserved, from a hard delete, which is explicit, permanent, and documented as usable only when no consumer depends on the schema. The same two-stage shape, with the second stage guarded by judgement rather than by a check.
* [crates.io](https://doc.rust-lang.org/cargo/commands/cargo-yank.html) never releases a published name for reuse, accepting permanent reservation as the price of reference integrity — the position Types Registry holds by default and departs from only under an explicitly enabled operation.

## Traceability

- **PRD**: [../PRD.md](../PRD.md)
- **DESIGN**: [../DESIGN.md](../DESIGN.md)
- **ADR-0001**: [0001-cpt-cf-types-registry-adr-storage-identity-query-model.md](./0001-cpt-cf-types-registry-adr-storage-identity-query-model.md)
- **ADR-0005**: [0005-cpt-cf-types-registry-adr-type-schema-revisions.md](./0005-cpt-cf-types-registry-adr-type-schema-revisions.md)
- **ADR-0006**: [0006-cpt-cf-types-registry-adr-registered-instance-revisions.md](./0006-cpt-cf-types-registry-adr-registered-instance-revisions.md)
- **ADR-0011**: [0011-cpt-cf-types-registry-adr-managed-external-boundary.md](./0011-cpt-cf-types-registry-adr-managed-external-boundary.md)
- **ADR-0012**: [0012-cpt-cf-types-registry-adr-write-path-admission-protocol.md](./0012-cpt-cf-types-registry-adr-write-path-admission-protocol.md) — decides the write path this job uses, and the general dry-run mode of which the dry run described here is one application.

This decision directly addresses:

* `cpt-cf-types-registry-fr-lifecycle` - supplies the only mechanism that physically removes registry state, and keeps it out of any retention policy.
* `cpt-cf-types-registry-fr-id-resolution` - names the single exception to the identifier non-rebinding guarantee, and bounds it to one local transaction.
* `cpt-cf-types-registry-fr-register-schemas`, `cpt-cf-types-registry-fr-register-instances` - bound the retention of admitted content: unbounded by policy, removable only by this operation, and therefore unremovable wherever it is disabled. §One operation records why no content-only removal is offered instead.
* `cpt-cf-types-registry-fr-minor-version-profile` - constrains which minors a purge may release, so that releasing an identifier cannot reoccupy a point in a compatibility sequence and leave a major with an unestablished step.
