---
status: accepted
date: 2026-07-26
decision-makers: Constructor Fabric Steering Committee
---

# Tenant Availability Dependency Propagation

**ID**: `cpt-cf-types-registry-adr-tenant-availability-evaluation`

## Table of Contents

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Scope](#scope)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Availability-blocking relationships](#availability-blocking-relationships)
  - [Propagation semantics](#propagation-semantics)
  - [P1 implementation latitude](#p1-implementation-latitude)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Admission snapshot](#admission-snapshot)
  - [Live resolution dependencies](#live-resolution-dependencies)
  - [Live semantic closure](#live-semantic-closure)
- [More Information](#more-information)
  - [Why the semantic model is recorded even though P1 does not exercise it](#why-the-semantic-model-is-recorded-even-though-p1-does-not-exercise-it)
  - [What would change the P1 latitude](#what-would-change-the-p1-latitude)
- [Traceability](#traceability)

<!-- /toc -->

## Context and Problem Statement

Types Registry returns the authoritative Tenant Availability State for a concrete entity and tenant. Lifecycle, visibility, tenant enablement when applicable, and live external-source assertions determine whether an entity is unavailable in its own right. This ADR addresses a different question: what happens to an already admitted entity when something it depends on is unavailable to that tenant.

Admission and runtime availability are separate concerns. Admission validates a Type Schema against its base chain and references, validates a Registered Instance against its conforming schema, and records the dependency revisions used. DESIGN may also materialize a resolved effective schema so that ordinary resolution does not physically read every dependency. Those choices establish that the entity was valid when admitted; they do not by themselves decide whether later tenant unavailability of a dependency affects the admitted entity.

For example, if a base Type Schema becomes disabled for tenant `A`, there are three coherent answers for a derived Type Schema:

* it remains available because admission certified it against the base;
* it remains available when its materialized effective schema can be returned without reading the base;
* it becomes unavailable because the base remains part of its semantic contract.

The decision must be explicit. Otherwise a storage optimization such as materializing an effective schema silently determines public availability semantics.

## Scope

This ADR decides:

* whether availability of an admitted entity changes when one of its dependencies becomes unavailable;
* whether that behavior follows admission snapshots, physical resolution needs, or semantic dependencies;
* which known registry relationships belong to the selected model;
* whether propagation is transitive and how its direction and cycles are interpreted;
* whether materialization may change the observable verdict.

This ADR does not decide:

* who computes the Tenant Availability State, which is a Types Registry responsibility established by the PRD;
* lifecycle transitions (ADR-0008);
* ownership, visibility, and non-disclosure behavior (ADR-0009);
* external-source state and failure semantics (ADR-0002);
* the Tenant Enablement State vocabulary or mutation API;
* the exact SDK/REST result and reason representation;
* storage, closure materialization, indexing, caching, or invalidation algorithms, which belong to DESIGN;
* how an owning gear handles an existing runtime object whose registry entity is unavailable.

## Decision Drivers

* `AVAILABLE` must have one stable meaning across entity kinds and storage implementations.
* Admission-time validity and current tenant policy must not be conflated.
* Materializing an effective schema is an implementation optimization and must not silently redefine public behavior.
* Tenant policy must have a defined meaning when an entity is used indirectly through a derived schema, reference, Instance, or Alias.
* A dependent becoming unavailable must not make a healthy dependency unavailable.
* Transitive propagation creates a potentially wide blast radius and invalidation set, so it must be an intentional contract rather than an accidental graph traversal.
* P1 write-time invariants may make some dependency failures unreachable in normal managed state, but the public semantic must remain stable when post-P1 tenant enablement is introduced.

## Considered Options

* **Admission snapshot** — availability follows what admission certified.
* **Live resolution dependencies** — availability follows what the current request must physically read.
* **Live semantic closure** — availability follows the semantic contract, whatever the storage does.

## Decision Outcome

Chosen option: **Live semantic closure.**

Tenant availability propagates transitively through outgoing availability-blocking relationships. A materialized effective schema, cached target, retained revision, or other precomputed artifact is an implementation optimization and does not sever those relationships.

The reason for propagation is tenant policy over the complete semantic contract, not a claim that every target must be physically fetched during ordinary resolution.

Admission snapshot was rejected because it would let a tenant continue using a disabled contract indirectly through an already admitted dependent and would require a special rule for Aliases. Live resolution dependencies was rejected because changing the resolution implementation could then change the public availability verdict without any product-policy change.

### Availability-blocking relationships

A directed relationship `E → T` is availability-blocking when `T` contributes to the semantic contract required to use `E`. If tenant policy does not permit use of `T`, it must not be bypassed by using `T` indirectly through `E`.

Applying this rule to the relationships known today:

| Relationship | Blocking | Why |
|---|---|---|
| Registered Instance → its conforming Type Schema | Yes | The schema defines the meaning and validity of the registered value. |
| Type Schema → each base in its GTS derivation chain | Yes | Inherited constraints remain part of the derived contract. |
| Type Schema → targets of `$ref` in its content | Yes | Referenced definitions are inlined and contribute to the effective contract. |
| Type Schema → targets named by `x-gts-ref` in its content | **No** | The keyword constrains an Instance value, but the named entity contributes no content to the schema's semantic contract. |
| P2 Alias → its target | Yes | An Alias owns no target content; using it means using its target. |
| Target → entities that depend on it | **No** | This is traversal against the dependency direction. A dependent's state says nothing about whether its target can be used. |
| Entity → its version-family siblings | **No** | Family membership does not make one member part of another member's semantic contract. |

The rule and the classifications above are normative. A new relationship kind must be explicitly classified by applying the same semantic-contract test; it does not become blocking merely because it is stored in the dependency graph.

**Every blocking relationship holds between two Managed Entities.** ADR-0011 closes the managed–external boundary in both directions, so no blocking edge crosses it: a Managed Entity has no external target, and an Externally Managed Entity has no managed one. Were the second direction permitted and listed as blocking, one relationship kind would be evaluated live by a source while every other was evaluated against managed storage; the closed boundary leaves one mechanism for one semantic. The availability of an Externally Managed Entity is asserted live by its owning source under ADR-0002 and composes with nothing stored here.

### Propagation semantics

Evaluation applies only after the subject entity has passed the visibility boundary. For a visible entity `E` and tenant `t`:

```text
available(E, t)
  = E has no unavailable input of its own for t
    AND
    no entity reachable from E through outgoing availability-blocking edges
    has an unavailable input of its own for t
```

Consequently:

* propagation follows outgoing edges from a subject to the targets it requires;
* propagation is transitive;
* incoming edges are relevant to impact analysis and invalidation, not to the subject's verdict;
* a healthy cycle is available;
* if any member of a cycle has an unavailable input of its own, every member that can reach it through blocking edges is unavailable.

These are semantic requirements, not a requirement to perform recursive single-hop reads for every request.

### P1 implementation latitude

P1 has no managed tenant enablement override, and the invariants around it turn out to close the question entirely rather than merely narrowing it. A blocking target cannot be `DELETED` while its dependent lives, because deletion is refused while a registered dependent exists. It cannot be invisible to a tenant that sees the dependent either, because admission requires the target to be resolvable, so the target's owner subtree contains the dependent's. And ADR-0011 keeps every blocking edge inside managed storage.

The one state that would escape those invariants is a tenant moving within the hierarchy, which ADR-0009 places out of scope. So in P1 no dependency can make a visible entity unavailable, and availability reduces to the entity's own lifecycle and visibility.

Types Registry therefore materializes no dependency closure and traverses none when computing a verdict. This is the latitude this ADR grants, taken in full: it does not change the selected semantic model, and introducing a new availability input later — a managed tenant enablement override, or tenant relocation entering scope — must not change what `AVAILABLE` means or require consumers to reinterpret existing results. What it would require is traversal, over the direct dependency edges that are retained regardless.

### Consequences

* Disabling a base, referenced schema, conforming schema, or Alias target for a tenant makes its semantic dependents unavailable for that tenant.
* Tenant policy cannot be bypassed by wrapping a disabled entity in a derived schema, Registered Instance, resolution-bearing reference, or Alias.
* A verdict can change without mutation of the subject entity, so an entity resource version alone cannot validate a cached resolution result.
* A change can affect every entity that reaches the changed target through blocking edges. DESIGN must provide an implementation and invalidation strategy that meets the lookup NFR.
* Materialized and on-demand resolution must return the same Tenant Availability State for the same registry and tenant state.
* Availability of a widely used base is independent of the health of its dependents because propagation never follows incoming edges.
* An unavailable result is not necessarily a defect; it may be an intentional consequence of tenant policy or a staged rollout.
* Exact reason selection, target disclosure, and the distinction between an unavailable verdict and an operation failure remain contract-design concerns governed by the visibility and external-source decisions.

### Confirmation

This decision is confirmed when:

* a Registered Instance becomes unavailable when its conforming Type Schema is unavailable;
* a Type Schema becomes unavailable when any base or `$ref` target in its transitive semantic closure is unavailable, while an unavailable entity named only by `x-gts-ref` does not affect the verdict;
* no blocking relationship crosses the managed–external boundary in either direction, and the availability of an externally managed entity is obtained live from its source rather than composed with stored managed state;
* a P2 Alias becomes unavailable when its target is unavailable;
* a materialized effective schema and an on-demand equivalent produce the same verdict;
* an unavailable dependent does not make its target unavailable;
* an unavailable version-family sibling does not affect another member;
* a healthy blocking cycle is available, while an unavailable input in the cycle propagates to every member that reaches it;
* two tenants may receive different verdicts for the same entity under the same registry revision;
* a verdict change caused by a target does not require mutation of the subject entity;
* a P1 implementation optimization based on write-time invariants remains observationally equivalent to the live semantic-closure model.

## Pros and Cons of the Options

### Admission snapshot

Successful admission certifies a content-bearing entity against the exact dependency state used at that time. After admission, the entity's Tenant Availability State depends on its own current inputs, not on the current Tenant Availability State of its dependencies.

Dependencies still matter for initial admission, for dependency-revision freshness and revalidation, for activation of a new base or referenced revision, and for deletion safety and impact analysis.

Under this option, disabling a base or `$ref` target for a tenant does not disable an already admitted dependent for that tenant. An Alias requires a separate exception because it owns no target content and cannot resolve from an admission snapshot.

* Good, because ordinary availability depends only on the subject's own current state.
* Good, because dependency closure traversal and transitive cache invalidation are unnecessary for availability.
* Good, because disabling a widely used target does not create a runtime cascade.
* Bad, because tenant policy on a target can be bypassed through an already admitted dependent.
* Bad, because `AVAILABLE` does not establish that the tenant may currently use the subject's complete semantic contract.
* Bad, because Alias resolution needs a separate live-target rule.

### Live resolution dependencies

Dependency availability propagates only when ordinary resolution must consult the target at request time. A target whose content has been materialized into the subject's stored representation does not block the subject.

Under the current design direction this could make an Alias depend on its live target while allowing a Type Schema with a materialized effective schema, or a Registered Instance with a stored admitted value, to remain available without their dependencies. It makes availability describe the current resolution plan: changing between eager materialization, joins, caches, and on-demand resolution can change which dependencies affect the public verdict.

* Good, because it performs live dependency checks only where the current resolution path needs them.
* Good, because materialized entities avoid dependency reads.
* Bad, because public availability semantics depend on storage and resolution strategy.
* Bad, because equivalent semantic relationships behave differently across entity kinds.
* Bad, because replacing materialization with on-demand resolution can change public behavior without a product-policy change.

### Live semantic closure

Dependency availability propagates when the target is part of the semantic contract required to use the subject, regardless of whether the target must be physically read during the current request. Materialization may remove reads from the hot path, but it does not sever semantic dependencies, so disabling a target for a tenant disables every entity that semantically depends on it, transitively.

* Good, because `AVAILABLE` has one meaning across entity kinds and implementations.
* Good, because tenant policy applies to indirect as well as direct use.
* Good, because materialization and caching remain behavior-preserving optimizations.
* Bad, because a target-state change can have a wide transitive blast radius.
* Bad, because closure maintenance and cache invalidation add implementation complexity.
* Bad, because operators must understand that disabling a shared base or reference disables its semantic dependents.

## More Information

### Why the semantic model is recorded even though P1 does not exercise it

This model has no observable P1 effect. As §P1 implementation latitude shows, write-time invariants leave no state where a dependency makes a visible entity unavailable. No closure is materialized or traversed.

Recording the semantics is still necessary. Otherwise storage behavior defines them implicitly, and the first new availability input — managed tenant enablement under `cpt-cf-types-registry-fr-tenant-enablement`, or tenant relocation — could silently change what `AVAILABLE` means for existing consumers.

### What would change the P1 latitude

Two developments retire the "no traversal needed" argument without touching the selected model. Managed tenant enablement introduces an availability input that admission cannot have validated against, since it is set after admission. And tenant relocation, placed out of scope by ADR-0009 and carried as PRD open question 4, is the one state in which a visible entity's blocking target can become invisible to it. Either one makes the traversal real; both are served by the direct dependency edges DESIGN retains for deletion safety regardless, so neither requires new storage.

## Traceability

- **PRD**: [../PRD.md](../PRD.md)
- **DESIGN**: [../DESIGN.md](../DESIGN.md)
- **ADR-0002**: [0002-cpt-cf-types-registry-adr-external-source-live-delegation.md](./0002-cpt-cf-types-registry-adr-external-source-live-delegation.md)
- **ADR-0005**: [0005-cpt-cf-types-registry-adr-type-schema-revisions.md](./0005-cpt-cf-types-registry-adr-type-schema-revisions.md)
- **ADR-0008**: [0008-cpt-cf-types-registry-adr-managed-version-family-lifecycle.md](./0008-cpt-cf-types-registry-adr-managed-version-family-lifecycle.md)
- **ADR-0009**: [0009-cpt-cf-types-registry-adr-tenant-ownership-visibility-authority.md](./0009-cpt-cf-types-registry-adr-tenant-ownership-visibility-authority.md)
- **ADR-0011**: [0011-cpt-cf-types-registry-adr-managed-external-boundary.md](./0011-cpt-cf-types-registry-adr-managed-external-boundary.md) — closes the boundary in both directions, which is why every availability-blocking relationship here holds between two Managed Entities.

This decision directly addresses:

* `cpt-cf-types-registry-fr-tenant-availability` - defines live dependency propagation and its independence from materialization;
* `cpt-cf-types-registry-fr-ref-tracking` - classifies which tracked relationships participate in the semantic closure;
* `cpt-cf-types-registry-fr-cache-freshness-metadata` - establishes that a target-state change can invalidate a subject result without mutating the subject;
* `cpt-cf-types-registry-nfr-lookup-latency` - requires DESIGN to make live semantic-closure behavior meet the lookup budget without prescribing an implementation.
