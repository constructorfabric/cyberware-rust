---
status: accepted
date: 2026-07-25
decision-makers: Constructor Fabric Steering Committee
---

# Type Schema Evolution Compatibility for Managed GTS Type Schemas

**ID**: `cpt-cf-types-registry-adr-type-schema-evolution-compatibility`

## Table of Contents

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Scope](#scope)
- [Terminology](#terminology)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Backward-compatible evolution](#backward-compatible-evolution)
  - [The comparison baseline](#the-comparison-baseline)
  - [Content model is classified, not mandated](#content-model-is-classified-not-mandated)
  - [Reporting is confined to refusal](#reporting-is-confined-to-refusal)
  - [The whole-history statement can lapse, and the lapse is not aggregated](#the-whole-history-statement-can-lapse-and-the-lapse-is-not-aggregated)
  - [External Registry Sources](#external-registry-sources)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Require `BACKWARD` compatibility](#require-backward-compatibility)
  - [Require `FULL` compatibility](#require-full-compatibility)
  - [Make the mode configurable per version family or per identifier namespace](#make-the-mode-configurable-per-version-family-or-per-identifier-namespace)
- [More Information](#more-information)
  - [Industry Practice](#industry-practice)
  - [Relationship to the GTS specification](#relationship-to-the-gts-specification)
- [Traceability](#traceability)

<!-- /toc -->

## Context and Problem Statement

Types Registry must decide when a new definition may replace the current definition of the same managed GTS Type Schema identity.

[GTS specification](https://github.com/globaltypesystem/gts-spec) 0.13 defines Type Schema Evolution Compatibility by inclusion of accepted-instance sets: backward means `Valid(old) ⊆ Valid(new)`, forward means `Valid(new) ⊆ Valid(old)`, and full means the two sets are equal. It leaves the enforced mode to the implementation (§6, item 6) and requires a registry to validate each successive definition against its preceding definition before publication (§5.3, item 2).

Two other decisions in this gear determine what Types Registry needs from that vocabulary:

* ADR-0004 makes a **major-only** managed GTS ID a mutable logical entity. Successive definitions are internal revisions of one identifier rather than new identifiers, and a managed `$ref` is a floating reference to the current revision of the exact identifier it names. A minor-bearing identifier is immutable and nothing floats to it.
* ADR-0005 states that domain gears do not store a Type Schema revision in their rows. A stored Registry Reference resolves to the logical entity, and resolution returns the current revision.

Together these remove the mechanism that a message-schema registry relies on. There is no per-object schema pinning, so no reader ever reconciles against the exact definition a payload was written under. The current revision must therefore accept payloads written under every earlier revision.

ADR-0004 also admits a minor version on a managed identifier, and that changes the shape of the chain this ADR governs without changing its rule. A minor-bearing entity is **immutable**: it is admitted with one definition and never revised. So a major is either one mutable entity with a revision chain or a sequence of single-definition entities, and in the second case the edges of the chain run between entities rather than between revisions of one. The guarantee therefore spans a major rather than one identifier, and it is stated below in the terms that make it hold.

## Scope

This ADR decides:

* the compatibility strategy Types Registry follows when a managed Type Schema evolves;
* the comparison baseline for an admission candidate;
* how a Type Schema's content model affects its ability to evolve in place, and what Types Registry does about it;
* what is reported about a check, and on which results;
* what P1 owes when the compatibility relation itself changes meaning;
* what an External Registry Source must assert about its own evolution rules.

This ADR does not decide Type Derivation Compatibility, which is a separate relation governed by the GTS specification and `cpt-cf-types-registry-fr-validate-type-derivation`. It does not decide transitions between successive values of a registered GTS Instance (ADR-0006), revision storage and retention (ADR-0005), or optimistic concurrency.

## Terminology

| Term | Meaning |
|---|---|
| Type Schema major identity | One logical managed GTS Type Schema identity within one major version, for example `gts.acme.crm.customer.type.v1~`. Where ADR-0004's minors are in use, one major holds several such identities — `v1.0~`, `v1.1~` — each a logical entity of its own. |
| Schema revision | One immutable admitted definition of one logical Type Schema identity. |
| Current revision | The admitted revision of one logical entity returned by ordinary resolution. It is the comparison baseline for a candidate of that same entity; the baseline of a minor-bearing candidate is the current revision of a different entity, the preceding minor. |
| Comparison baseline | The definition a candidate is checked against: the current revision of the same logical entity where that entity is major-only and mutable, or the definition of the preceding minor of its major where the candidate is minor-bearing. |
| Effective schema | The authored schema after `allOf` chain aggregation, as the platform-approved GTS implementation materializes it. |
| Backward compatible | `Valid(previous) ⊆ Valid(candidate)`, per GTS specification §4.3. |
| Operational guarantee | A claim about producer or reader behaviour, casting, or default materialization. It is not schema compatibility and is reported separately. |

## Decision Drivers

* A stable GTS Type Schema identity must provide a predictable guarantee to consumers that hold a Registry Reference rather than a revision.
* Floating references plus no revision in domain rows mean the current revision is the only definition a consumer ever validates against.
* P1 has no owning-gear Validation Hooks, so any guarantee that depends on consumer-code behaviour cannot be enforced by the registry.
* Admission cost should not grow with the number of retained revisions.
* The registry must not restate or contradict semantics the GTS specification already defines.

## Considered Options

* Require `BACKWARD` compatibility.
* Require `FULL` compatibility.
* Make the mode configurable per version family or per identifier namespace.

`FORWARD` is not among them. Under it the current revision may accept fewer instances than an earlier one, so a payload written earlier could be rejected by the definition a consumer resolves today. With floating references and no revision stored in domain rows, that is the one outcome the registry must never permit.

## Decision Outcome

Chosen option: **Types Registry follows a `BACKWARD` compatibility strategy and compares an admission candidate against one baseline — never against history.** That baseline is the candidate's own current revision where its entity is major-only, and the current definition of the preceding minor of its major where the candidate is minor-bearing.

### Backward-compatible evolution

A candidate is admissible only when `Valid(baseline) ⊆ Valid(candidate)` under the platform-approved GTS implementation, where the baseline is the current revision of the same logical entity when it is major-only, and the definition of the preceding minor of its major when the candidate is minor-bearing (ADR-0004). Because deciding that inclusion is not always possible for arbitrary JSON Schema, a candidate whose compatibility the implementation cannot establish is rejected rather than published; Types Registry fails closed and never treats an undecided check as a pass.

The one exception is per candidate and reaches only the second case: ADR-0004's `force` waives the cross-minor check, and can never waive the intra-entity one. §*The comparison baseline* records why that asymmetry is sound rather than convenient.

This is exactly the guarantee the architecture requires. Because a domain object carries no revision and a `$ref` floats, the current revision is validated against payloads and references created under any earlier revision. Backward compatibility is the property that makes that safe.

Both edges are the same `Valid(baseline) ⊆ Valid(candidate)`, so no second mode is introduced and no verdict means something different depending on which produced it. They differ only in who is protected: within one identifier a dependent that is *carried onto* the new revision, across a minor boundary one that *chooses* to move — which is exactly why only the second may be waived.

One profile is exempt, and it is exempt rather than weaker. ADR-0015 gives major version 0 the meaning of an unstable Type Schema: no mode is enforced for it, so it carries no whole-history guarantee to protect. That exemption is safe for everything decided here only because the same ADR forbids a stable schema from including one through `$ref` or deriving from one — without that rule a floating resolution-bearing reference would carry the exemption upward into a chain this ADR does guarantee.

`FULL` is not enforced. Under GTS 0.13 full compatibility requires the accepted-instance sets to be equal, so the only admissible in-place changes would be annotations such as `description`, `examples`, and `default`. That would make ADR-0004's mutable logical entity inert: no content update could ever preserve the GTS ID, and every change would require a new major identity.

`FORWARD` is not enforced. Types Registry has no platform requirement to guarantee that an older reader accepts a newer payload, and it has no mechanism to establish which readers exist.

### The comparison baseline

An admission candidate is compared against one baseline. It is not compared against every retained revision.

This is sufficient because backward compatibility is set inclusion, and set inclusion is transitive. If every admitted revision satisfies `Valid(rev_n) ⊆ Valid(rev_n+1)`, then `Valid(rev_1) ⊆ Valid(rev_N)` holds across the whole chain without any earlier revision being re-examined. GTS specification §5.3, item 2 states the same baseline as a registry requirement: validate each successive definition against its preceding definition before publication. The specification defines no transitive compatibility mode, and this ADR does not introduce one.

**Where ADR-0004's minors are in use the chain runs through them and the argument is unchanged.** A minor-bearing entity is immutable, so a major is either one mutable entity with a revision chain or a sequence of single-definition entities, never both; either way the definitions of one major form one sequence whose every adjacent pair is an edge this check established:

```text
major-only     v1#1  ≤  v1#2  ≤  v1#3  …      each vs its own current revision
minor-bearing  v1.0  ≤  v1.1  ≤  v1.2  …      each vs the minor before it
```

Transitivity then yields the guarantee at the granularity a consumer cares about: **the current revision of the highest minor of a stable major accepts every instance ever accepted anywhere in that major**, provided every edge it composes was actually established — which excludes a step admitted under `force` and excludes any edge whose verdict was computed under semantics the relation has since left behind. Admission still reads one baseline, so its cost is independent of how many minors and revisions the major holds.

The argument requires the chain to stay intact, which ADR-0005 guarantees — every revision is checked against the then-current revision, no admitted revision is removed, and the current pointer never moves backward. Across minor boundaries one further rule keeps it intact, and ADR-0004 owns it because it is a property of identity rather than of compatibility: **the minors of a major are contiguous and open at `M.0`.** Without it the sequence branches at a shared predecessor, the endpoints stop being comparable, and the guarantee above is not established.

**Contiguity, not mere ordering, makes the baseline sound under concurrency.** The check runs before the commit transaction. If the baseline were *the highest admitted minor below the candidate*, concurrent admission could change it between selection and commit:

1. Two successors select the same lower member.
2. One commits.
3. The other still satisfies ordering but was never checked against the newly committed predecessor.

Contiguity instead names the baseline in the candidate identifier: `vM.n~` always uses `vM.(n-1)~`. At commit, admission only rechecks that this identifier still exists as `ACTIVE` or `DELETED`. Deletion does not unaccept the predecessor's instances, so a deleted predecessor remains the baseline.

**Only the intra-entity half of the chain is structural**, because only there is a consumer carried onto the new revision by a floating reference. The cross-minor edge underwrites no mechanism, which is why ADR-0004 permits `force` to waive it for one candidate and why nothing may waive the other. A major containing a forced step still has a well-formed sequence; what it no longer has is the guarantee stated above about its endpoints.

The erase-and-reintroduce hazard follows from the same property rather than from a separate rule. Under specification §4.5, adding and removing a property have opposite backward verdicts in each content model: in a closed model addition is backward compatible and removal is not, in an open model removal is backward compatible and addition is not. A property therefore cannot be dropped and later reintroduced under a different schema without one of the two steps failing the enforced mode.

Consequently Types Registry does not compare a candidate against every retained revision, needs no cap on retained revisions for compatibility purposes, and has admission cost independent of history size.

This depends on the platform-approved GTS implementation actually following the 0.13 semantics, since the superseded rules report some incompatible pairs as compatible and a candidate-versus-baseline check is **not** sufficient under them. That alignment is a prerequisite of this decision rather than an independent task: if the implementation does not provide it, the baseline chosen here is unsound and no amount of registry code repairs it.

Because a checker upgrade can change the verdict for an unchanged pair of schemas, each admitted revision records the GTS specification version and platform GTS implementation version **in force when it was admitted**. Where a comparison happened those are the rules that produced its verdict; where none did — a first admission, an `M.0`, a major-0 candidate — they are admission-engine provenance and assert nothing about a verdict, since there was none. Without the record the registry cannot identify which chains were validated under superseded rules. The field belongs to the revision record defined by ADR-0005.

That record exists because **the transitivity argument holds only within one version of the compatibility relation.** A chain whose edges were admitted under different semantics proves nothing about its endpoints, since an edge accepted under superseded rules may not satisfy the current relation at all.

**P1 deliberately does not decide the response to changed checker semantics.** The condition cannot arise until the first specification revision or checker correction after launch; every initial edge uses one rule version. A policy chosen now would therefore lack its key inputs: what changed and how many majors are affected.

GTS 0.13 versus 0.12 illustrates the shape: corrected OP#8 verdicts affect open content models, enums, and `const` fields. Recorded version differences cannot distinguish affected from unaffected chains; only revalidation can. What to do with failures is a policy question with no P1 consumer.

Recording the versions is therefore the whole of the P1 obligation, and it is the part that cannot be added afterwards: a verdict that was never attributed to a rule version cannot be attributed to one retroactively. ADR-0014 protects that record from the second way a chain could span two semantics, by pinning the dialect for the life of a major.

**Until that response exists, a semantic change can silently unestablish a major's whole-history statement.** New revisions remain sound against their immediate baselines under the new rules; older edges retain their original verdicts.

The exposure does not compound: each new edge still satisfies `Valid(rev_n) ⊆ Valid(rev_n+1)`, narrowing rather than widening the instances at risk. It also does not clear, and no operation reports it. PRD records this risk.

### Content model is classified, not mandated

Types Registry does not require a managed Type Schema to declare `additionalProperties: false`.

The content model does decide whether a Type Schema can evolve in place, and it decides it per object level. In an open object, adding an optional property is not backward compatible, because the previous definition already accepted arbitrary values under that property name. An open effective level therefore cannot gain properties in place at all, and such a change needs a new major identity; a closed effective level admits additive changes.

Both are legitimate. A schema meant to be extended by derived types — in particular an `x-gts-abstract` base — needs an open level precisely so that derived types can declare properties there, and closing it would defeat its purpose under specification §3.1. A blanket closure rule would therefore be wrong at exactly the levels that exist in order to be extended.

The platform's schema-generation toolchain already resolves this per level rather than per schema: a generated Type Schema closes the root of a base type and the level that carries a derived type's own properties, while leaving each declared extension container open for the next derivation level. That is the closed-envelope-with-designated-open-containers shape recommended by specification §4.4.1, so a generated Type Schema is evolvable in place at the levels its owner actually edits. Types Registry relies on that property of the schemas it receives, not on how they are produced.

The property does not extend to every level of such a schema. An object level introduced by a generated property subschema is open unless its author closes it, and hand-authored or externally supplied schemas carry no guarantee at all. Both cases are admitted normally, and the consequence arrives when a change at the affected level is refused.

Instead of a rule, Types Registry classifies:

* every object level of the fully resolved effective schema is classified as open, closed, or partially open, **per level rather than per document**, because in the closed-envelope shape the level deciding evolvability is inside an extension container rather than at the root. A partially open level stays that rather than being forced into either category;
* the classification **MUST** be computed after GTS reference resolution, because a derived Type Schema may be closed at a level only through the `$ref` to its base. It is not a test of the authored `additionalProperties` keyword: GTS specification §4.4 requires the classification to come from the resolved schema, because `unevaluatedProperties`, `patternProperties`, `propertyNames`, a schema-valued `additionalProperties`, or a conjunctive subschema reached through `allOf` or `$ref` can all change whether undeclared properties are accepted;
* the classification is an input to the verdict, not an output of the registry. It is not reported on an admitted candidate — where it applies, it applies as the refusal of a later change, whose diagnostics name the offending level. §*Reporting is confined to refusal* records why.

Owners that want in-place evolution **SHOULD** use the closed envelope with designated open containers described in specification §4.4.1: the closed envelope carries evolvability, the declared open containers carry extensibility by derivation. Generated Type Schemas obtain that shape for the levels the toolchain owns; an owner who introduces further object levels **SHOULD** close them explicitly, otherwise those levels cannot gain properties in place. How a given authoring toolchain expresses closure is a platform authoring convention, not a registry rule. Closing a trait sub-schema would break `allOf` composition and is not expected.

### Reporting is confined to refusal

**A refused candidate carries structured diagnostics naming the cause and the offending schema location. A candidate that was admitted carries nothing about the check.** The forward-direction result **MAY** ride along as advisory diagnostics, since the implementation computes both directions in one call.

The asymmetry is not an economy. Everything a successful result could have said is already available to the caller, or unreachable, or a fold this decision set declined elsewhere:

* the **enforced mode** follows from the identifier the caller submitted — backward unless the last segment carries major 0;
* the **verdict** on a success can only be *compatible*, *waived*, or *no baseline to compare against*: fail-closed rejects *incompatible* and *undecided*, `force` is a flag the caller sent and is read back through provenance, and *no baseline* means the candidate is the first minor of its major, which its identifier states;
* **per-level evolvability** describes a future limitation rather than this candidate. An owner learns it where it applies: the refusal of the change that runs into it names the level. Reporting it a publication earlier was the original intent and is dropped, because it would need a stored per-item payload that nothing else in the write path has.

Operational guarantees are reported separately from schema compatibility, as specification §4.3 requires. P1 does not certify tolerant-reader contracts, constrained-producer conventions, casting, or default materialization, and **MUST NOT** present any of them as a schema-compatibility result.

### The whole-history statement can lapse, and the lapse is not aggregated

A major carries the statement *the highest minor accepts every instance ever accepted here* only while every edge of its chain was actually established. Two things break that: a semantic change of the relation, which invalidates edges admitted under superseded rules and whose handling is deferred above; and a `force`-admitted minor, where an edge was deliberately never established (ADR-0004).

Neither is reported as an aggregate over the major, and for the second that is a decision rather than an omission: ADR-0004 declined the aggregate outright, since it would be "a fold over facts the caller is already reading" — `compat_forced` on each member's provenance — and a consumer crossing several minors consults each. The first is not reported at all, which §*What the platform does* records as an accepted limitation.

### External Registry Sources

An External Registry Source remains authoritative for its own evolution rules, and Types Registry **MUST NOT** relabel a source guarantee, of any strength, as the platform guarantee. A consumer distinguishes the two from `origin` on the read result.

Nothing more is asked of a source about its evolution — no declared guarantee, and no platform check against one. There is nothing such a declaration could gate: the only consumer it would protect is a managed Type Schema referencing an external one, which ADR-0011 makes unrepresentable, so no managed verdict can depend on how a source evolves its definitions. A gear that reads an external entity directly decides for itself what it needs of it, as it does for any content the platform does not govern.

A managed Type Schema cannot reference an externally managed one. ADR-0011 prohibits that direction in P1, and one of its four reasons is the evolution guarantee: specification §4.5 makes the compatibility of a referenced-type change depend on the effective resolved schemas, so a managed schema would inherit the weakest guarantee in its reference closure and could not claim backward compatibility unless the owning source asserted backward monotonicity per revision. The rule stated here therefore applies to a managed reference closure that is entirely managed, which is the only closure P1 admits.

### Consequences

* Every managed Type Schema whose major version is 1 or higher follows the same backward-compatible evolution rule. Major 0 is the single exemption, quarantined from the rest by ADR-0015.
* One mode covers both edge kinds. A minor boundary changes which definition is the baseline and nothing else, so no result or diagnostic distinguishes a cross-minor verdict from a major-only intra-entity revision verdict — except that only the former may have been waived, which ADR-0004 records.
* The guarantee a consumer can state is per major rather than per identifier, and only for a stable major every edge of which was established: the highest minor accepts everything that major ever accepted, provided no member of it was force-admitted and no edge predates a semantic change of the relation. A major-0 major offers no such guarantee at all, and a semantic change of the relation can withdraw it from a major whose edges predate that change. In a major-only major that sentence degenerates to the previous one, since the major has exactly one member and nothing there can be waived.
* Admission cost is independent of retained history, so retention growth is a storage question only.
* An open effective level is admitted normally, but cannot later gain properties in place; a change at that level requires a new major identity, and the owner learns it from the refusal of that change, whose diagnostics name the level.
* Generated Type Schemas are evolvable in place at the levels that carry their own properties, so ADR-0004's mutable logical entity is the normal case for platform gears rather than an exception. Object levels the toolchain does not own are the residual hazard and are addressed by an authoring convention, not by a registry rule.
* Alignment of the platform GTS implementation with GTS 0.13, including per-level content-model classification computed on the resolved effective schema, is a P1 prerequisite of this decision. DESIGN enumerates the capabilities it covers and records verifying them as an implementation prerequisite.
* Every comparison this ADR governs is between two documents of one dialect, because ADR-0014 pins it for the lifetime of a logical entity. Otherwise `Valid(S)` would be dialect-relative on both sides and the set-inclusion argument would not compose.
* OP#8 as specified compares two definitions addressed by distinct GTS Type Identifiers, which ADR-0004's in-place replacement model never produces. Types Registry therefore uses the document-level compatibility entry point of the platform GTS implementation and owns its own revision addressing, as specification §4.2 anticipates.
* Where a contract needs old readers to accept new payloads, the owner uses a new major identity.

### Confirmation

This decision is confirmed when:

* admission rejects a candidate that is not backward compatible with its baseline, and admits one that is;
* admission compares against one baseline only, and a test with a long revision history shows admission cost independent of history length;
* `v1.1~` is checked against `v1.0~` and rejected when it is not backward compatible with it, while a revision of a major-only entity is checked against its own current revision;
* `v1.1~` and `v1.2~` submitted concurrently over `v1.0~` never both commit without `v1.2~` having been compared against `v1.1~`, and a deleted `v1.1~` is still the baseline of `v1.2~` rather than being skipped for `v1.0~`;
* an instance valid under any retained revision of any minor of a stable, unforced major validates against the current revision of that major's highest minor, and the test is not asserted for a major-0 major or for one containing a forced edge;
* a `force`-admitted candidate is accepted without the cross-minor check and no attempt to force a revision of a major-only entity succeeds;
* the erase-and-reintroduce sequence is rejected: a revision that drops a property and a later revision that reintroduces it under a different schema cannot both be admitted;
* admission classifies content model per object level from the resolved effective schema, admits open-topped and `x-gts-abstract` schemas without objection, and reports no classification on an admitted candidate;
* the classification is exercised against a schema closed only by `unevaluatedProperties`, one closed only through an `allOf` branch, one closed only through a resolved GTS `$ref` to its base, and one made partially open by a schema-valued `additionalProperties`;
* an additive update to a Type Schema in the closed-envelope shape is admitted at the level carrying its own properties, while the same update at an open level is rejected as not backward compatible, and the diagnostic identifies the open level rather than the document root;
* trait-composed schemas are accepted when the effective outer level is closed even though trait sub-schemas are not;
* a refused candidate carries the cause and the offending schema location, optionally with the forward-direction result labelled advisory, while an admitted candidate carries no verdict, no enforced mode, and no per-level classification — and neither does any read;
* no compatibility result presents a tolerant-reader, casting, or default-materialization claim as schema compatibility;
* every admitted revision records the specification and implementation versions in force at its admission — including revisions admitted with no comparison, where they attribute an engine and not a verdict — and a chain whose checked edges span two versions is identifiable from that record alone;
* the platform GTS implementation passes GTS 0.13 conformance for open-model property addition and removal and for both enum directions;
* no result presents a source's own evolution behaviour as the platform guarantee, and no admission or federation check reads a source assertion about it.

## Pros and Cons of the Options

### Require `BACKWARD` compatibility

* Good, because it is exactly the guarantee the floating-reference and no-revision-in-rows architecture requires.
* Good, because admission cost is constant in history size and no revision cap is needed.
* Good, because it matches the default strategy of the closest industry analogue.
* Bad, because the platform gives no forward-direction guarantee; a contract whose old readers may receive new payloads must use a new major identity.
* Bad, because whether a given Type Schema can actually evolve in place depends on the content model its author chose, so the guarantee is uniform while its practical reach is not.
* Bad, because its sufficiency depends on the implementation being aligned with GTS 0.13, and is reopened by any later semantic change to the relation, whose handling P1 records the enabling data for and otherwise defers.

### Require `FULL` compatibility

* Good, because producers and consumers could be deployed in any order without coordination.
* Bad, because under GTS 0.13 full compatibility is equality of accepted-instance sets, so it admits only annotation changes and semantics-preserving refactorings. Applied platform-wide it would make ADR-0004's mutable logical entity unusable and force a new major identity for every semantic change, defeating the stable-Registry-Reference goal of ADR-0001.
* Bad, because it would be a platform-wide answer to a per-type question: most contracts do not need it, and the ones that do are asking for immutability rather than for a different evolution policy.

### Make the mode configurable per version family or per identifier namespace

GTS 0.13 offers this latitude explicitly: §4.3 notes that "systems may enforce different modes for different identifier namespaces", and §6 leaves the enforced mode to the implementation. Types Registry could expose the mode as policy — selected for a version family at its first admission, or attached to a namespace — rather than fixing one platform-wide value.

* Good, because a contract that genuinely needs producers and consumers deployed in any order could ask for `FULL` without imposing it on every other type. The family record introduced by ADR-0008 already provides durable per-family policy storage, so the objection that this option requires new state is weaker than it was when it was first considered.
* Bad, because the configurable set could only be `BACKWARD` and `FULL`. `FORWARD` remains unavailable for the architectural reason above, so configurability would not unlock the case that usually motivates it — an event contract wanting old readers to accept new payloads — and `FULL` under 0.13 is nearer to immutability than to an alternative evolution policy.
* Bad, because a managed `$ref` floats, so the guarantee a Type Schema offers is the weakest mode in its reference closure. Under one platform mode that holds automatically; under configurable modes it becomes a property that has to be computed, pinned at admission, and rechecked whenever any member of the closure changes. That is the problem this ADR currently confines to External Registry Sources, imported into managed entities.
* Bad, because derivation crosses ownership: a tenant deriving from a platform base cannot hold its own type to a stronger guarantee than the base whose mode another owner controls.
* Bad, because the mode would be effectively immutable once chosen. Strengthening says nothing about history, and weakening retracts a guarantee consumers may already rely on — so it would be selected before the consumers who care about it exist.

Adopting it would first require deciding where the mode attaches and how precedence works if namespaces are used, which introduces a second prefix-policy system over the identifier space that Source Claims already partition; whether the effective mode of a reference closure is computed and pinned; what a mode means for registered Instances, whose successive values have no compatibility relation at all; and whether the mode participates in the resolution cache validator, since changing it changes the meaning of a result.

## More Information

### Industry Practice

* [Confluent Schema Registry](https://docs.confluent.io/platform/current/schema-registry/fundamentals/schema-evolution.html) defaults to `BACKWARD` and documents it as non-transitive. Its transitive strategies compare against all previous versions, while the non-transitive strategy compares only against the most recent version. Its JSON Schema rules reason about open and closed content models in the same way GTS 0.13 does, and its documented advice for an evolvable schema is to close the content model from the start.
* [Kubernetes API deprecation policy](https://kubernetes.io/docs/reference/using-api/deprecation-policy/) forbids removing or changing an API element within an existing API version and requires a new version for incompatible evolution. This is the closer analogue for a platform type registry, where consumers are built against a version as a whole rather than reconciling per payload.
* [Protocol Buffers](https://protobuf.dev/programming-guides/proto3/#updating) permits field removal but permanently reserves the field identity, showing that the hazard is identity reuse rather than removal itself.

### Relationship to the GTS specification

This ADR selects behaviour inside the latitude GTS 0.13 leaves to implementations. It requires backward compatibility as defined by the specification and adds no authoring rule the specification does not already recommend. It does not redefine compatibility, transitivity, or derivation.

It declines one part of that latitude deliberately. §4.3 permits an implementation to enforce different modes for different identifier namespaces; Types Registry enforces one mode — BACKWARD — wherever a mode is enforced at all, for the reasons recorded against the configurable option above. Two exceptions are named elsewhere and are not namespace-scoped: a major 0 declares no mode (ADR-0015), and a `force`-admitted minor leaves one edge of an enforced major unestablished (ADR-0004). Should that be revisited, the specification permits the change without amendment.

## Traceability

- **PRD**: [../PRD.md](../PRD.md)
- **DESIGN**: [../DESIGN.md](../DESIGN.md)
- **ADR-0002**: [0002-cpt-cf-types-registry-adr-external-source-live-delegation.md](./0002-cpt-cf-types-registry-adr-external-source-live-delegation.md)
- **ADR-0004**: [0004-cpt-cf-types-registry-adr-gts-minor-version-identity-evolution.md](./0004-cpt-cf-types-registry-adr-gts-minor-version-identity-evolution.md) — makes a major-only identifier a mutable logical entity, admits minors as immutable single-definition entities, owns the contiguity rule that keeps the chain governed here from branching at a minor boundary and makes its baseline immune to concurrent admission, and owns the `force` waiver of the cross-minor edge.
- **ADR-0005**: [0005-cpt-cf-types-registry-adr-type-schema-revisions.md](./0005-cpt-cf-types-registry-adr-type-schema-revisions.md)
- **ADR-0014**: [0014-cpt-cf-types-registry-adr-managed-type-schema-dialect-profile.md](./0014-cpt-cf-types-registry-adr-managed-type-schema-dialect-profile.md)
- **ADR-0011**: [0011-cpt-cf-types-registry-adr-managed-external-boundary.md](./0011-cpt-cf-types-registry-adr-managed-external-boundary.md) — forbids a managed Type Schema from referencing an externally managed one, so the guarantee defined here applies to a reference closure that is entirely managed.
- **ADR-0015**: [0015-cpt-cf-types-registry-adr-major-zero-unstable-profile.md](./0015-cpt-cf-types-registry-adr-major-zero-unstable-profile.md) — exempts major 0 from the mode enforced here, and quarantines it so the exemption cannot reach a chain this ADR guarantees.
- **GTS specification**: [Global Type System](https://github.com/globaltypesystem/gts-spec) §4.1–§4.5, §5.3, §6

This decision directly addresses:

* `cpt-cf-types-registry-fr-validate-schema-compat` - defines the guarantee enforced for managed Type Schema evolution and the baseline it is checked against.
* `cpt-cf-types-registry-fr-ref-tracking` - a floating reference remains valid because the current revision stays backward compatible with the revision a dependent was admitted against.
* `cpt-cf-types-registry-usecase-validate-type-evolution` - defines what CI and admission must prove before an in-place change is accepted.
