---
status: accepted
date: 2026-08-04
decision-makers: Constructor Fabric Steering Committee
---

# Unstable Major-Zero Profile for Managed Type Schemas

**ID**: `cpt-cf-types-registry-adr-major-zero-unstable-profile`

## Table of Contents

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Scope](#scope)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [What major 0 exempts, and what it does not](#what-major-0-exempts-and-what-it-does-not)
  - [The quarantine rule](#the-quarantine-rule)
  - [Why the identifier is the right carrier](#why-the-identifier-is-the-right-carrier)
  - [Registered Instances](#registered-instances)
  - [Nothing is stored](#nothing-is-stored)
  - [Graduation is an ordinary registration](#graduation-is-an-ordinary-registration)
  - [What the platform does not promise](#what-the-platform-does-not-promise)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Keep one enforced mode and iterate with new majors](#keep-one-enforced-mode-and-iterate-with-new-majors)
  - [Keep one enforced mode and iterate with delete, purge, re-register](#keep-one-enforced-mode-and-iterate-with-delete-purge-re-register)
  - [A configurable compatibility mode per family or namespace](#a-configurable-compatibility-mode-per-family-or-namespace)
  - [A stored stability flag on the entity](#a-stored-stability-flag-on-the-entity)
  - [An unstable profile keyed on major 0 in the identifier](#an-unstable-profile-keyed-on-major-0-in-the-identifier)
- [More Information](#more-information)
  - [Industry Practice](#industry-practice)
  - [Relationship to the GTS specification](#relationship-to-the-gts-specification)
- [Traceability](#traceability)

<!-- /toc -->

## Context and Problem Statement

ADR-0003 enforces `BACKWARD` compatibility wherever a mode is enforced on a managed Type Schema, and rejects a candidate whose compatibility the implementation cannot establish. That is the correct rule for a published contract, and the wrong one for a contract still being designed.

A type under active development changes shape, and the platform offers an author two ways to do it. A new major identity is one: it churns the identifier, issues a new Registry Reference, and leaves the abandoned major `ACTIVE`, because ADR-0008 defers deprecation past P1 and nothing retires a major automatically. The purge of ADR-0013 is the other: it is disabled in production by default, and it works by *releasing the identifier*, so by construction it cannot serve a type that anything already references — releasing the name is precisely what rebinds a stored reference to an unrelated entity.

Neither serves the case this decision is about: a type whose shape is not settled while consumers already exist. In that state the author knows the change is breaking, accepts responsibility for it, and needs the identifier and the reference to survive it.

The GTS grammar already admits major 0 — `major = "0" | positive-integer`, and §10 uses `v0` in its own wildcard examples — and the managed identity profile has never said anything about it. A `v0` identifier is therefore registrable today and carries exactly the same enforcement as any other major. The question this ADR answers is not whether major 0 may be used, but whether it should mean something.

## Scope

This ADR decides:

* whether major 0 marks a distinct evolution profile for managed Type Schemas;
* which admission checks that profile exempts, and which it leaves in force;
* how an unstable entity is prevented from weakening a stable one through a floating reference;
* whether registered Instances participate;
* whether the profile is stored, and how an entity leaves it;
* what the platform does and does not promise for an unstable entity.

This ADR does not decide the enforced mode itself or the comparison baseline (ADR-0003), managed identity semantics (ADR-0001, ADR-0004), version-family lifecycle (ADR-0008), the dialect profile (ADR-0014), or the write-path protocol through which the checks run (ADR-0012). It does not change anything for Externally Managed Entities, whose evolution rules their source owns (ADR-0002).

## Decision Drivers

* A type under active development must be able to change shape while its GTS Identifier and Registry Reference stay stable, because those are what consumers have already persisted.
* Purge releases the identifier and so cannot serve a referenced type; a new major forces every consumer to re-point on every reshape rather than once.
* A managed `$ref` floats to the current revision (ADR-0004), so the guarantee a Type Schema offers is the weakest guarantee in its resolution closure. Any relaxation must be prevented from leaking upward.
* Responsibility must be localizable. An owner accepting risk for its own type must not be able to transfer it silently to an owner who accepted nothing.
* ADR-0003 examined and rejected configurable compatibility modes for five reasons. A new relaxation must not reintroduce them.
* Whatever marks the relaxation must be legible to a consumer without a second lookup, since a consumer that cannot see the risk cannot accept it — which is also why no registry field has to carry it.
* The check must be decidable at admission from state Types Registry owns (`cpt-cf-types-registry-principle-local-authority`).

## Considered Options

* Keep one enforced mode and iterate with new majors.
* Keep one enforced mode and iterate with delete, purge, re-register.
* A configurable compatibility mode per version family or per identifier namespace.
* A stored stability flag on the entity.
* An unstable profile keyed on major 0 in the identifier.

## Decision Outcome

Chosen option: **an unstable profile keyed on major 0 in the identifier.** A managed Type Schema whose own last segment carries major 0 evolves without the enforced compatibility check of ADR-0003, and no stable schema may include one in its resolution closure or derive from one.

### What major 0 exempts, and what it does not

The exemption is narrow and is stated as a list so that nothing is waived by implication.

**Exempted.** Type Schema Evolution Compatibility, in both shapes a major 0 can take. A content revision of a **major-only** `v0~` entity is admitted whatever its relation to the current revision: narrowing, widening, incomparable, or undecidable. A **minor-bearing** `v0.n~` entity takes no content revision at all — immutability is not waived — and instead the next contiguous minor `v0.(n+1)~` is admitted with no cross-minor check against it. ADR-0003's fail-closed rule for an unprovable verdict applies to neither, because there is no verdict to establish.

Nothing else about the relation reaches a v0 entity either: ADR-0003 records the specification and implementation versions of every verdict, and a v0 entity has no verdict to attribute, so a later reckoning with a semantic change of the relation has nothing here to reckon with.

**Not exempted, and each for its own reason.**

* **Type Derivation Compatibility.** `Valid(derived) ⊆ Valid(base)` is a property of the identifier chain, not of evolution — the specification separates the two relations precisely so they can be reasoned about apart (§4.1 against §4.2). Waiving it would make a chained identifier state a substitutability that does not hold, which is a lie in the one place GTS encodes meaning in the string itself.
* **Dependent revalidation.** ADR-0005 requires every affected registered dependent to remain valid before a new revision becomes current, and that rule stands. It is consequently still possible for a v0 reshape to be refused: if a v0 derived type would stop satisfying its base, the base's reshape fails. The remedy is to fix or delete the dependent, not to weaken the chain.
* **The dialect profile.** ADR-0014 pins Draft-07 and pins it for the life of the logical entity. A v0 entity is not exempt: the dialect governs what `Valid(S)` even means, and the derivation check above still needs both sides evaluated under one semantics.
* **The minor-version rules of ADR-0004.** A major-0 identifier may carry a minor wherever any identifier may. The exemption reaches the *check* on the cross-minor edge and not the contiguity rule, which governs which identifiers may exist rather than what a candidate is compared against — so a major-0 major still opens at `v0.0~` and still admits no gaps; nor does it reach immutability, so a major-0 minor is admitted once like any other and reshaping means publishing `v0.2~`. That is where the two profiles compose: an author wanting to reshape freely *and* leave existing dependents in place gets both. It is also why `force` is refused on a major-0 candidate — there is no check left for it to waive.
* **Everything else on the admission path.** GTS validity and the rest of the managed identity profile, reference resolvability, deletion safety, ownership, registration authority, and the write-path preconditions of ADR-0012 all apply unchanged. An unstable type is unstable in its shape, not in its governance.

### The quarantine rule

**A managed Type Schema whose own last segment carries major 1 or higher MUST NOT include an entity whose last segment carries major 0 in its resolution closure or derive from one.** The rule applies to:

* a `$ref` target;
* the immediate derivation base.

`x-gts-ref` is outside this rule because it validates an Instance value without resolving or inlining the named entity. This includes exact and patterned major-0 identifiers, `gts.*`, and relative JSON pointers. None creates a dependency or guarantee over the target.

The whole resolution closure follows by induction. Any transitive path to v0 must contain a stable entity with a direct resolution-bearing dependency on v0, which admission refuses. ADR-0003 uses the same argument shape to derive a whole-history guarantee from candidate-versus-baseline checks.

The induction's base case holds because **no registry predates the rule**. The release that introduces the quarantine check is the release that first persists a managed entity at all, so at enablement there is no admitted dependency to inherit and nothing to grandfather — the property the rule maintains starts out true rather than being asserted over existing state. Two consequences worth naming: an implementation MUST NOT enable the rule against a registry populated by a build that had the storage but not the check, since those dependencies were admitted under no rule at all; and a later release that re-establishes the rule over an existing registry has to demonstrate the base case rather than assume it, for which a check over stored dependencies is the obvious instrument.

Without quarantine, the profile leaks. A `$ref` floats, so reshaping unstable `address.v0~` can redefine the accepted-instance set of stable `customer.v1~` that references it — **without any authored revision of `customer.v1~`**. DESIGN §1.1 recomputes its current-state projection when the dependency advances.

ADR-0005 dependent revalidation only proves that `customer.v1~` remains *valid*. It does not prove compatibility with what that type accepted yesterday. One owner could therefore withdraw another owner's guarantee, the opposite of localized responsibility.

The rule needs no new machinery or state. Admission reads the immediate derivation base from the candidate identifier and extracts `$ref` targets from its content. The target major is present in each target identifier, so no target document has to be loaded for the quarantine decision.

This is a static property of the submission, checked beside ADR-0014's dialect profile. ADR-0011's closed boundary makes it well-posed: every resolution-bearing target named by a managed document is Managed and its identifier is platform-interpretable.

The relation is one-way and that is deliberate. An unstable type **MAY** build on a stable one — weaker on stronger is sound, and it is the normal case, since a new type under development usually derives from a published base. Only stronger-on-weaker is refused.

### Why the identifier is the right carrier

ADR-0003 considered making the compatibility mode configurable and rejected it. Four of its five objections do not reach this decision, and the difference in every case is that the profile lives in the identifier rather than in stored policy.

* *"It would require new state."* It requires none. The profile is a substring of a column the registry already holds.
* *"The mode would be effectively immutable once chosen, and selected before the consumers who care about it exist."* It is immutable here too, but it is not hidden: it is in the identifier a consumer holds, reads in logs, and stores as a reference. And it is escapable — graduation to v1 is an ordinary registration.
* *"It introduces a second prefix-policy system over the identifier space that Source Claims already partition."* There is no policy and no prefix system. There is one field of one segment.
* *"A managed `$ref` floats, so the effective mode of a closure has to be computed, pinned at admission, and rechecked whenever any member changes."* The quarantine rule replaces that computation with a prohibition, and the prohibition cannot drift: stability is a property of an identifier, an identifier never changes, and a v0 entity never becomes a v1 entity — `v1` is a different entity in the same family. A closure that was uniform at admission stays uniform for the life of its members.

The fifth objection — *"derivation crosses ownership: a tenant deriving from a platform base cannot hold its own type to a stronger guarantee than the base whose mode another owner controls"* — is answered by the quarantine rule rather than dodged: the situation it describes is unrepresentable, because a stable derived type cannot have an unstable base.

### Registered Instances

The profile is defined for Type Schemas, and two consequences follow for Instances.

**Major 0 carries no meaning on a registered Instance identifier and MUST be refused there.** ADR-0006 defines no compatibility relation between successive Instance values; they are already free to change. Marking them “unenforced evolution” would make major 0 mean two things and destroy the inference a reader makes from it.

Admission therefore adds one comparison to the managed Instance identity profile, alongside:

* no explicit UUID tail (ADR-0001);
* no minor version (ADR-0004);
* no major 0.

ADR-0004 refuses minors on Instance identifiers for the same reason while admitting them on Type Schema identifiers under every prefix.

**A registered Instance MUST NOT conform to a v0 Type Schema.** ADR-0006 otherwise blocks a schema revision that makes an affected Instance invalid. Applying that rule to v0 would restore the evolution block this ADR removes. Waiving it would leave invalid Instances behind the platform-wide invariant that a current Instance value is valid against its schema's current revision — the invariant that lets Types Registry store no revalidation record at all.

Refusing the combination keeps that invariant true. The accepted cost is that a control-plane type and its Instances cannot be co-developed under v0; such a type starts at v1.

### Nothing is stored

Types Registry **MUST NOT** store the profile as a column. It is derivable from `entity.gts_id`, which the registry retains in full, so a stored copy would be a second authority over a derived fact and would need an invariant to keep the two from diverging — which `cpt-cf-types-registry-principle-derive-not-store` prohibits. This is the same reasoning ADR-0014 applied to the declared dialect, and it has the same consequence: `database.sql` does not change.

### Graduation is an ordinary registration

An unstable type becomes stable by being registered at v1. No operation, no transition, and no migration is required, because the existing model already covers it: `family(gts.cf.b.c.d.v0~)` and `family(gts.cf.b.c.d.v1~)` are the same family key under ADR-0004, ADR-0008 permits several members of one family to be `ACTIVE` simultaneously and in any order, and ADR-0009 fixes one owner for the whole family through the family record. The v0 member stays `ACTIVE` until its owner deletes it.

Graduating a type that is used as a **base** is expensive, and the cost belongs to ADR-0004 rather than to this decision. A family key holds every preceding segment exactly as written, so `A.v0~B.v1~` and `A.v1~B.v1~` are different families: moving a base out of the unstable profile orphans everything derived from it, which must be re-registered under new identifiers and receives new Registry References. The unstable profile is therefore appropriate for leaf types and expensive for bases, and authoring guidance must say so.

### What the platform does not promise

Three limits are stated here so that they are not discovered later.

**A v0 type gives no protection to runtime data held by other gears.** Types Registry cannot see domain objects, and the reshape of a v0 schema may leave live rows no longer conforming to it. This introduces no new class of hazard: `cpt-cf-types-registry-fr-lifecycle` already permits an `ACTIVE` type to be deleted while live domain data conforms to it, and records that as a stated P1 limitation rather than a guarantee. Deleting a contract under live data is strictly more destructive than reshaping it. What changes is only that the risk is now legible in the identifier.

**Nothing prevents a production consumer from depending on a v0 type.** The marker is a convention backed by review, not an invariant backed by a check. P1 has no lever to make it one: managed tenant enablement is deferred to P2 by `cpt-cf-types-registry-fr-tenant-enablement`, and the grant model of `cpt-cf-types-registry-fr-registration-authority` governs writing rather than reading. A deployment that wants the marker enforced on the read path needs a capability P1 does not have.

**Discovery cannot currently exclude unstable types.** A GTS wildcard has no negation and `GET /entities` has no stability filter, so a catalogue view that wants published contracts only cannot express it. Adding the filter later is additive; it is recorded as an open question rather than built now.

### Consequences

* An author with an unsettled contract keeps one identifier and one Registry Reference across any number of breaking reshapes, and pays a single re-point at graduation instead of one per reshape.
* Admission compares the candidate's immediate derivation base and `$ref` targets against the quarantine beside ADR-0014's dialect check. A major is readable from an identifier, so no target document has to be loaded.
* The managed **Instance** identity profile acquires a third narrowing, so its last segment carries no explicit UUID tail, no minor version, and no major 0. On a Type Schema identifier the second is not refused anywhere; ADR-0004 admits a minor under every prefix.
* A v0 reshape can still be refused, by dependent revalidation, when a derived type would stop satisfying its base. This is derivation compatibility doing its job and is not a defect of the profile.
* No storage change, no migration, and no new operation.
* The contract gains no value in any enumeration. ADR-0003 confines compatibility reporting to refusal, and a v0 entity has nothing to refuse on this axis, so admitting one simply says nothing about compatibility — which serves the concern below better than a special value would: there is no bare verdict to be mistaken for a guarantee if there is no verdict.
* A control-plane type and its registered Instances cannot be co-developed under the profile.
* Introducing the profile is **schema-additive**: it changes no stored row, and it needs no deployment step of its own. The quarantine rule's base case comes from the release boundary — the storage and the check arrive together — so there is no existing closure to remediate and no condition on enablement.

### Confirmation

This decision is confirmed when:

* a content revision of a **major-only `v0~`** entity is admitted **when the non-exempt checks pass** whether it narrows, widens, or is incomparable to the current revision, including a pair that a comparison would have reported undecidable — which is never invoked, so no verdict is computed or reported; the same candidate against a `v1~` entity is admitted only in the widening case and rejected in the other three; and a content revision of a **minor-bearing `v0.n~`** entity is refused, immutability being unwaived;
* a v0 derived Type Schema that violates its base chain is rejected, proving derivation compatibility is not waived;
* a v0 Type Schema candidate declaring a dialect other than Draft-07 is rejected, proving ADR-0014 is not waived;
* a `v0.2~` candidate is admitted without any compatibility check against `v0.1~`, while opening major 0 at `v0.1~`, admitting `v0.2~` over a missing `v0.1~`, and revising `v0.1~` at all are each rejected — proving the exemption reaches the check and not the contiguity or immutability rules;
* admission rejects a v1 Type Schema carrying a `$ref` to a v0 target, and rejects a v1 identifier deriving from a v0 base, with diagnostics naming the offending reference;
* admission admits a v1 Type Schema whose `x-gts-ref` names a v0 entity exactly or through a pattern, as well as one using `gts.*` or a relative JSON pointer, proving that instance-value constraints are outside quarantine;
* admission accepts a v0 Type Schema that references and derives from v1 targets, proving the quarantine relation is one-way;
* a v0 reshape that would leave a v0 derived type no longer satisfying its base is refused by dependent revalidation;
* registration of a managed registered Instance whose own last segment carries major 0 is rejected;
* registration of a registered Instance conforming to a v0 Type Schema is rejected;
* no column of Types Registry storage holds a stability value, and no migration accompanies this decision;
* registering v1 of a family whose v0 member is `ACTIVE` succeeds, leaves the v0 member `ACTIVE`, and is refused for any owner other than the family's;
* an admission or Dry Run of a v0 entity reports no compatibility verdict at all, and a caller establishes that no mode applies from the identifier it submitted;
* an Externally Managed Entity whose source serves a v0 identifier is resolved and returned without any of these rules being applied to it.

## Pros and Cons of the Options

### Keep one enforced mode and iterate with new majors

* Good, because every managed entity carries one guarantee and a consumer never has to read the identifier to know which.
* Good, because it needs no decision at all.
* Bad, because every breaking reshape issues a new identifier and a new Registry Reference, so every consumer re-points on every iteration rather than once.
* Bad, because ADR-0008 defers deprecation past P1 and nothing retires a major, so an iterated type accumulates `ACTIVE` majors that are deletable only while nothing depends on them.

### Keep one enforced mode and iterate with delete, purge, re-register

* Good, because ADR-0013 already built it and it needs nothing new.
* Good, because the development stand keeps rehearsing production exactly, which is that ADR's central argument.
* Bad, because purge releases the identifier, which is a data-corruption primitive by ADR-0013's own description: deterministic derivation reproduces the reference and any row still holding it silently rebinds. It therefore cannot serve a type anything already references, which is the case this decision is about.
* Bad, because purge is disabled in production by default, so the relief is unavailable in the environment where consumers are most likely to already exist.

### A configurable compatibility mode per family or namespace

Examined and rejected by ADR-0003; retained here because the chosen option occupies the same design space.

* Good, because it would let a contract that genuinely needs `FULL` ask for it without imposing it platform-wide.
* Bad, for the five reasons ADR-0003 records: the configurable set could only be `BACKWARD` and `FULL`, the effective mode of a floating reference closure would have to be computed and rechecked, derivation crosses ownership, the mode would be effectively immutable but invisible, and a namespace-attached mode would be a second prefix-policy system over the identifier space.
* Bad, because it needs durable per-family policy state, which the chosen option does not.

### A stored stability flag on the entity

A boolean or enumeration column on `entity`, set at initial admission and immutable thereafter.

* Good, because it separates the concept from the version number, so a type could be marked unstable at any major.
* Good, because discovery could filter on it directly, which the chosen option cannot express.
* Bad, because it is invisible where it matters most. A consumer holds an identifier and a Registry Reference; under this option neither says anything, so the risk can only be learned by a lookup that a consumer has no reason to perform.
* Bad, because it stores a fact that would otherwise be derivable, which `cpt-cf-types-registry-principle-derive-not-store` prohibits, and it needs a migration and an immutability invariant that the chosen option gets from the identifier for free.
* Bad, because it reintroduces the computed-closure problem: with stability orthogonal to the identifier, a closure's composition is no longer fixed by construction and would have to be established and maintained.

### An unstable profile keyed on major 0 in the identifier

* Good, because the risk is legible in the value every consumer already holds, so accepting it is an informed act rather than an assumption.
* Good, because it needs no storage, no migration, no new operation, and no new state, and graduation falls out of the existing version-family model.
* Good, because the quarantine rule makes responsibility genuinely local: an unstable type can harm only entities whose owners also opted in.
* Good, because it is schema-additive: it stores nothing, migrates nothing, and has no deployment step — the quarantine base case comes from the release boundary rather than from a scan of existing state.
* Good, because it follows the convention every developer already knows from SemVer, Go modules, Cargo, and Kubernetes alpha versions, so it needs no explaining to the actors who will use it.
* Bad, because it consumes a major number for a meaning, so a genuine `v0` in the ordinary sense is no longer available. In practice the two coincide.
* Bad, because it cannot be applied to a settled type that later needs one breaking change: that case still needs a new major.
* Bad, because graduating a v0 **base** orphans everything derived from it, so the profile is materially cheaper for leaves than for bases.
* Bad, because nothing on the read path prevents production consumption; the marker is backed by review rather than by a check.

## More Information

### Industry Practice

Marking a not-yet-settled contract by a convention in its version string, rather than by policy state held beside it, is one of the most widely adopted conventions in software distribution. It appears in two spellings.

**Major zero, literally.**

* [Semantic Versioning 2.0.0](https://semver.org/#spec-item-4) is the normative source: major version zero "is for initial development", "anything MAY change at any time", and "the public API SHOULD NOT be considered stable". This decision adopts that meaning unchanged.
* [Go modules](https://go.dev/doc/modules/version-numbers) classify `v0.x.x` as "in development and unstable" and state that such a release carries no backward-compatibility guarantee. Go also shows the one place our model is stricter than the convention: a module keeps the same import path from `v0` through `v1` and only acquires a major suffix at `v2`, so graduating out of the unstable range costs a Go consumer nothing. Here `v0~` and `v1~` are different identifiers and therefore different Registry References, so graduation costs one re-point. The consolation is the one this ADR was written for — one re-point instead of one per reshape.
* [Cargo](https://doc.rust-lang.org/cargo/reference/semver.html) encodes the convention in the resolver itself: a `0.x` requirement is treated as compatible only within that minor, because the ecosystem assumes breaking changes land there. The signal is honoured by tooling with no registry check enforcing it — which is precisely the standing this decision gives it.

**The same idea under a different spelling.**

* [Kubernetes API versioning](https://kubernetes.io/docs/reference/using-api/#api-versioning) carries the stability level in the version string itself — `v1alpha1` — and defines it as software that "may contain bugs", whose support "may be dropped at any time without notice", whose API "may change in incompatible ways in a later software release without notice", and which is "recommended for use only in short-lived testing clusters". This is the closest analogue to what is decided here, because the marker travels inside the identifier a client uses rather than sitting in configuration beside it. Kubernetes additionally disables alpha APIs by default and requires them to be enabled explicitly in the API server — an enforcement lever P1 does not have, and the reason the corresponding limitation is stated plainly above rather than implied away.
* [Google AIP-181](https://google.aip.dev/181) makes stability an explicit, named property of an API component, with `alpha` describing something that "undergoes rapid iteration with a known set of users" and is expressed in the version string as `v1alpha`. It is the same choice of carrier, from the same body of guidance this gear already draws on for resource revisions and freshness validation.
* [Azure Resource Manager](https://learn.microsoft.com/en-us/azure/azure-resource-manager/management/resource-providers-and-types) uses dated API versions with a `-preview` suffix, again in the string a caller sends, with no compatibility guarantee and an independent retirement schedule.

**The closest product category does not use this model.** Confluent, AWS Glue, Google Pub/Sub, and Azure Event Hubs schema registries configure compatibility per subject. None encodes stability in the consumer-held identifier.

Borrowing from package and API versioning rather than schema registries is deliberate. Those registries govern message schemas resolved by producers at write time; other GTS Types also depend on a GTS Type through floating references.

One part of this decision has **no** precedent above. Cargo, Go, and npm allow stable artifacts to depend on unstable ones, leaving the exposure to author judgement.

Consumer opt-in is insufficient here. A floating `$ref` transfers instability to another owner who neither opted in nor sees a verdict. The quarantine rule is therefore a platform-specific addition.

### Relationship to the GTS specification

The specification permits this without amendment. §2.1 admits major 0 in the grammar and §10 uses it in its own examples. §4.2 leaves the publication shape for successive definitions to the implementation, and §6 item 6 makes the enforced Type Schema Evolution Compatibility mode — and the publication policy around it — implementation-defined, noting in §4.3 that "systems may enforce different modes for different identifier namespaces". Enforcing no mode for one major is inside that latitude.

The specification attaches no meaning to major 0 itself. This decision therefore adds a platform convention rather than reading one out of the specification, in the same way ADR-0004 narrowed the managed profile by constraining how minor versions are numbered and ADR-0001 by forbidding an explicit UUID tail. Should the convention prove valuable beyond this platform, it is a candidate for a specification change request rather than something to assume other GTS implementations share.

## Traceability

- **PRD**: [../PRD.md](../PRD.md)
- **DESIGN**: [../DESIGN.md](../DESIGN.md)
- **ADR-0001**: [0001-cpt-cf-types-registry-adr-storage-identity-query-model.md](./0001-cpt-cf-types-registry-adr-storage-identity-query-model.md) — the Registry Reference this profile exists to keep stable, and the precedent for narrowing the managed identity profile.
- **ADR-0003**: [0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md](./0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md) — the enforced mode this profile exempts, and the rejected configurable-mode option this decision is measured against.
- **ADR-0004**: [0004-cpt-cf-types-registry-adr-gts-minor-version-identity-evolution.md](./0004-cpt-cf-types-registry-adr-gts-minor-version-identity-evolution.md) — the version-family key that makes graduation an ordinary registration, and the base-graduation cost.
- **ADR-0005**: [0005-cpt-cf-types-registry-adr-type-schema-revisions.md](./0005-cpt-cf-types-registry-adr-type-schema-revisions.md) — dependent revalidation, which is not exempted and remains the one way a v0 reshape can be refused.
- **ADR-0006**: [0006-cpt-cf-types-registry-adr-registered-instance-revisions.md](./0006-cpt-cf-types-registry-adr-registered-instance-revisions.md) — why registered Instances neither carry the profile nor conform to a type that does.
- **ADR-0008**: [0008-cpt-cf-types-registry-adr-managed-version-family-lifecycle.md](./0008-cpt-cf-types-registry-adr-managed-version-family-lifecycle.md) — several members of a family may be `ACTIVE`, which is what lets v0 and v1 coexist during graduation.
- **ADR-0011**: [0011-cpt-cf-types-registry-adr-managed-external-boundary.md](./0011-cpt-cf-types-registry-adr-managed-external-boundary.md) — the closed boundary that makes the quarantine check decidable from local state.
- **ADR-0013**: [0013-cpt-cf-types-registry-adr-platform-purge-of-deleted-entities.md](./0013-cpt-cf-types-registry-adr-platform-purge-of-deleted-entities.md) — the alternative this decision complements rather than replaces.
- **ADR-0014**: [0014-cpt-cf-types-registry-adr-managed-type-schema-dialect-profile.md](./0014-cpt-cf-types-registry-adr-managed-type-schema-dialect-profile.md) — the closure check this one sits beside, and the precedent for not persisting a derivable profile.

This decision directly addresses:

* `cpt-cf-types-registry-fr-validate-schema-compat` - exempts major 0 from the enforced mode and bounds the exemption with the quarantine rule.
* `cpt-cf-types-registry-fr-gts-validation` - adds major 0 to the managed identity profile for Type Schemas and refuses it on registered Instance identifiers.
* `cpt-cf-types-registry-fr-register-schemas` - widens the admissible evolution of a managed Type Schema whose major is 0.
* `cpt-cf-types-registry-fr-register-instances` - refuses a registered Instance conforming to a v0 Type Schema.
* `cpt-cf-types-registry-fr-ref-tracking` - a dependency edge to a v0 target is admissible only from a v0 subject.
* `cpt-cf-types-registry-fr-lifecycle` - graduation adds a family member and changes the status of none.
