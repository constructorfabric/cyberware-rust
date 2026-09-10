---
status: accepted
date: 2026-08-09
decision-makers: Constructor Fabric Steering Committee
---

# GTS Minor-Version and Identity-Evolution Policy

**ID**: `cpt-cf-types-registry-adr-gts-minor-version-identity-evolution`

## Table of Contents

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Scope](#scope)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Managed identity rules](#managed-identity-rules)
  - [A minor is a reference-pinning boundary](#a-minor-is-a-reference-pinning-boundary)
  - [When a minor is admissible](#when-a-minor-is-admissible)
  - [The compatibility chain spans minors](#the-compatibility-chain-spans-minors)
  - [Breaking the chain deliberately: `force`](#breaking-the-chain-deliberately-force)
  - [What a managed version family is](#what-a-managed-version-family-is)
  - [Lifecycle of family members](#lifecycle-of-family-members)
  - [Reference and derivation rules](#reference-and-derivation-rules)
  - [Exact resolution versus patterns](#exact-resolution-versus-patterns)
  - [Externally managed entities](#externally-managed-entities)
  - [What counts as an identifier conflict](#what-counts-as-an-identifier-conflict)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Mandatory minor version, immutable entity](#mandatory-minor-version-immutable-entity)
  - [Mandatory minor version, mutable entity](#mandatory-minor-version-mutable-entity)
  - [No minor version, immutable entity](#no-minor-version-immutable-entity)
  - [No minor version, mutable logical entity, platform-wide](#no-minor-version-mutable-logical-entity-platform-wide)
  - [Per-major shape: mutable major-only, immutable minors](#per-major-shape-mutable-major-only-immutable-minors)
- [More Information](#more-information)
  - [Relationship to the GTS specification](#relationship-to-the-gts-specification)
  - [Industry Practice](#industry-practice)
- [Traceability](#traceability)

<!-- /toc -->

## Context and Problem Statement

GTS identifiers permit a major version and an optional minor version. Types Registry must decide whether managed GTS Type Schemas and registered GTS Instances require a minor version, allow each owning gear to choose whether to use one, or prohibit minor versions in the platform-managed identity profile.

The choice is coupled to identity mutability:

* an immutable minor-versioned entity can publish a new compatible definition under a new minor GTS ID;
* a mutable major-only entity can publish a compatible definition without changing its GTS ID;
* an incompatible definition must remain distinguishable from the old contract through a new major GTS ID.

Optional minor versions appear flexible but give otherwise similar GTS IDs different stability semantics. A `$ref` may be immutable and pinned in one family but mutable and floating in another. A major-only GTS ID can also be both a concrete identifier and, in pattern matching, a selector that covers minor-versioned candidates. This ambiguity affects exact resolution, deterministic Registry References, caches, derived types, federation, and version-membership queries.

Only a minor can express one useful property. Under a mutable major-only entity, every `$ref` and derivation base floats to the current revision. Publishing a compatible change therefore gives it to every dependent, whether requested or not. Backward compatibility makes that safe; it does not make it *wanted*.

An owner may instead want a separately addressable safe upgrade that existing dependents adopt deliberately. A new major provides non-adoption but discards the compatibility statement, because it denotes an incompatible change. A minor fills that gap: separately addressable, checked as a safe upgrade, and never applied automatically.

This ADR establishes the platform-facing identity policy. The enforced Type Schema Evolution Compatibility mode is decided by ADR-0003. Internal revision representation and retention are decided separately by ADR-0005 and ADR-0006. The lifecycle of the members of a version family — how many may be usable at once, and whether deprecation exists — is decided by ADR-0008.

## Scope

This ADR decides:

* whether minor versions are allowed in newly registered managed GTS identifiers, and under what control;
* what a minor version means when it is admitted, what it is checked against, and whether that check may be waived — and, if it may, what governs whether a deployment permits the waiver at all;
* whether the minors of one major may be numbered with gaps, and which minor may open a major;
* whether compatible content updates preserve the GTS ID;
* when a new major GTS ID is required;
* what constitutes a managed version family, including for chained identifiers with more than one versioned segment;
* whether derived types and references are rewritten automatically;
* how exact identity resolution differs from version-membership and wildcard matching;
* how the policy applies to externally managed entities;
* what counts as a conflict between two definitions submitted under one managed GTS identifier.

This ADR does not define database tables, revision retention, rollback APIs, or the concrete compatibility algorithm.

## Decision Drivers

* Domain gears need stable Registry References and should not migrate stored references for every compatible contract update.
* A GTS ID must have one predictable stability meaning for platform-managed entities, and where two meanings exist the identifier itself must say which one applies.
* An owner must be able to publish a compatible successor without applying it to existing dependents, and the platform must be able to state that the successor is a safe upgrade.
* Platform contracts declared by the Gears framework must have exactly one shape, so that no consumer of a platform type has to reason about which profile it is under.
* Automatic cloning of derived types or rewriting of `$ref` targets would publish owner-visible contracts without owner intent.
* Compatibility and dependency checks should be centralized in Types Registry rather than reimplemented by every gear.
* Exact identifier resolution must remain deterministic and must not silently select a different minor version.
* External Registry Sources may already have authoritative versioning semantics that Types Registry must preserve.
* The platform-approved `gts-rust` implementation currently supports minor versions and assumes versioned IDs are normally immutable; adopting a different managed profile must be explicit.

## Considered Options

* Mandatory minor version, immutable entity.
* Mandatory minor version, mutable entity.
* No minor version, immutable entity.
* No minor version, mutable logical entity, platform-wide.
* Per-major shape: a mutable major-only entity, or immutable minors.

## Decision Outcome

Chosen option for Types Registry-managed entities: **one major of a version family is either a single mutable major-only entity or a gap-free sequence of immutable minor-bearing ones opening at `M.0`, decided by its first admitted member; a minor is the boundary at which references stop floating; and the cross-minor compatibility check may be waived per candidate with `force`, which is recorded.**

Major-only remains the recommendation and the overwhelmingly common case. It is what platform documentation tells a vendor to use, what everything under `gts.cf.*` uses by a lint over its own source, and what an author gets by not writing a minor. A minor is for the owner who has a reason for one — a catalogue that is already minor-versioned, or a contract whose consumers must not be carried onto a successor — and the reason is theirs to have rather than an operator's to grant.

### Managed identity rules

* A managed GTS ID identifies one logical entity. A major-only identifier names a **mutable** one whose successive definitions are retained revisions; a minor-bearing identifier names an **immutable** one, admitted with a single revision that never changes.
* A newly registered managed GTS Type Schema **may** carry a minor version in the last segment of its identifier, with no region of the namespace excepted. P2 managed GTS Aliases use the same rule when introduced.
* Newly registered managed **registered Instances must** omit the minor version from the last segment, for ADR-0015's reason rather than a new one: a minor there would be a marker with nothing to mark. Nothing is lost — an Instance of a minor-versioned type carries the minor in a *preceding* segment, `gts.A~acme.crm.order.type.v1.2~acme.thing.v1`.
* A content update that is backward compatible under ADR-0003 preserves the same GTS ID and Registry Reference. It is available only to a major-only entity; a minor-bearing one has no content update, and its owner publishes the next minor instead.
* A backward-incompatible Type Schema change requires a new major GTS ID. A minor is not an alternative to it: the check across a minor boundary is the same check, so an ordinary minor carries no change a revision could not. The single exception is the `force` waiver below, which is available on a minor precisely because it is *not* available on a revision — and which withdraws a statement rather than granting a licence.
* Registered Instance content may change under the same GTS ID according to ADR-0006; schema compatibility terminology does not apply to successive Instance values.
* Where an owner writes no minor, the profile is exactly what it was: major-only, one mutable logical entity per major.

### A minor is a reference-pinning boundary

This is what a minor means, and it is the only thing it means.

A `$ref` or derivation base names an entity by its exact identifier, including its minor. Under ADR-0003, a reference to a mutable entity floats to its current revision.

`x-gts-ref` is not a pinning boundary. It validates an instance value against an identifier pattern without resolving an entity, so it may accept identifiers from any minor or major while creating no dependency on them. GTS §9.6 also permits `gts.*`, which accepts any valid identifier, and relative JSON pointers that name a location in the holder's document.

**A minor-bearing entity is not mutable.** It is admitted once with one revision, and its authored content never changes. Publishing the next minor creates a different identifier and Registry Reference; existing dependents do not move. Adoption requires the dependent's owner to re-author its reference.

Mutability is consequently a property of the shape rather than a setting on it, and the two shapes divide cleanly:

| | major-only `v1~` | minor-bearing `v1.0~` |
|---|---|---|
| Content changes by | a new revision in place | publishing `v1.1~` |
| Checked against | its own current revision | the current definition of `v1.0~` |
| Reaches existing dependents | yes, floating; ADR-0005 revalidates them first | never |
| Registry Reference | survives every change | one per minor; a dependent re-points to adopt |

That is the property the introduction names and a major-only profile cannot express, and it is bought with exactly the thing major-only was protecting. ADR-0001's guarantee is unchanged in what it promises — one identifier always derives to one reference — but in a minor-bearing major the identifier a dependent holds is *replaced* more often, and that is the deliberate trade.

**It is content-immutability, not immutability.** Two things still move even for an entity that will never take another revision: the **resolved effective schema** is recomputed whenever a floating dependency of its own advances, so `resolution_fingerprint` moves and consumer caches invalidate; and the owner may still **delete** it, which advances `resource_version`. A caller wanting the resolved form to hold still needs its whole reference closure pinned, which the registry does not offer and this ADR does not claim.

The registry offers no way to write a reference that floats *across* minors. Such a reference would be a `latest`-resolution mode, which *Exact resolution versus patterns* below refuses and ADR-0008 refuses again; more to the point it would undo the only reason the minor exists.

### When a minor is admissible

A minor is admissible on **any** managed Type Schema identifier. There is no configuration, no flag, no grant, and no reserved region — an owner who writes a minor has chosen the pinning boundary, and the choice is visible in the identifier to everyone who reads it.

**The platform's own contracts are major-only by convention, and the registry does not enforce it.** Every GTS Type Schema and registered Instance declared in the platform repository — everything under `gts.cf.*` — is major-only, and that is checked at build time by an architecture lint over the source declaring it, never at admission. The lint reaches what that repository declares and nothing else, so a `gts.cf.*` identifier arriving through the API is admitted like any other.

The division is worth contrasting with the closures the registry *does* hold. `cpt-cf-types-registry-fr-registration-policy` leaves a region closed to tenant ownership because ownership is fixed at admission and a platform contract wrongly admitted as tenant-owned is repaired only by deletion and purge — that rule protects a guarantee. A minor on a platform type breaks no guarantee at all; the only thing wrong with it is that it is not how this codebase writes identifiers, and house style belongs in a lint over the source, which fails the author rather than the deployment.

Deployment configuration governing where a minor may be written was considered and rejected — see *Sub-choices within the selected option*, below.

**No mixing, one major at a time.** Within one **major**, either every member carries a minor or none does. The first admitted member fixes the shape.

No policy column or family scan is needed once contiguity applies. Exactly one identifier decides each direction:

* a minor-bearing major opens at `vM.0~`, so a minor-bearing candidate is refused while `vM~` exists;
* a major-only candidate is refused while `vM.0~` exists.

Both are keyed lookups under the ownership-check lock. Kind exclusivity still reads one arbitrary member because kind belongs to the family, not the major.

**The grain is the major and not the family, because the compatibility chain is.** A new major starts a chain of its own and inherits nothing from the one before it, so there is no property of `v1` that `v2` would be preserving by copying its shape. A family may therefore hold a major-only `v1~` beside a minor-bearing `v2.0~`, `v2.1~`. What a consumer pays for that is one look at an identifier, and it pays that anyway: adopting a new major is a re-pointing whatever the shapes are.

### The compatibility chain spans minors

The one definition of a minor-bearing entity is checked under the enforced mode of ADR-0003 against the **current definition of the preceding minor of its major**. There is no second baseline case, because there are no later revisions to have one:

```text
v1.0  ≤  v1.1  ≤  v1.2  …
      ↑ each checked against the one before
```

The consumer-facing statement is that **moving from any minor to any higher minor of the same stable major is a safe upgrade, provided every step between was established**, which follows by transitivity of set inclusion exactly as it does along the revision chain of a major-only entity. A major-0 major carries no such statement, since no mode is enforced there.

**That statement is a convenience and not an invariant, and the difference decides what may be relaxed.** Inside one identifier the backward guarantee is structural: a `$ref` floats, so a consumer is *carried onto* the new revision and the current one must accept what the old one accepted. Across a minor boundary nothing is carried — references pin, Instances conform to an exact minor, no dependent is revalidated — so no mechanism in the registry rests on the cross-minor edge. It exists to tell a human that a move they are choosing to make is safe.

**One rule is nevertheless required**, because a statement about a sequence needs the sequence to have an order. The obvious form — *strictly higher than every minor already admitted* — is not enough, because it lets a concurrent pair through unchecked; *Sub-choices within the selected option*, below, works the interleaving through.

**The rule is therefore contiguity, and it comes in two halves that are only sound together.**

* **No gaps.** A minor `vM.n~` with `n > 0` is admissible only while `vM.(n-1)~` is admitted.
* **The first minor of a major is `M.0`.** Nothing else may open a Minor-Bearing Major.

The second half is not tidiness: without it the rule reads *either `n-1` is admitted or the major is empty*, and "the major is empty" is a fact about state again. Pinning the opening minor removes the last state-dependent clause.

**Contiguity makes the candidate name its baseline instead of selecting one from state.** The submitted `vM.n~` identifies `vM.(n-1)~`, so concurrent admission cannot change which definition must be checked. This removes the race by construction rather than by locking a family snapshot.

At commit, admission only re-asks the keyed question *does `vM.(n-1)~` exist?* Concurrent delete-and-purge may have removed it during validation. An absent predecessor fails retryably, matching `cpt-cf-types-registry-fr-two-phase-init` for a base not yet registered.

Strict ordering is then a consequence rather than a second rule: the admitted minors of a major are always exactly `{0..k}`, so the only admissible new one is `k+1`, and every lower number is already occupied by an identifier that cannot be registered twice.

**Existence counts a deleted predecessor.** `vM.(n-1)~` satisfies the rule whether it is `ACTIVE` or `DELETED`, and its retained definition is the baseline in either case. Deletion does not unaccept the instances that minor accepted, so skipping a deleted predecessor and checking against the one below it would reintroduce the branch through an ordinary lifecycle act. This is the property ADR-0008 leaves intact: it declined to **store or expose** a newest member, and neither a keyed existence test nor a retained definition is that.

**Purge is not an exception, and it is the only operation that could have been.** Releasing an identifier removes a predecessor. Without another rule, purging `v1.1~` could leave `v1.2~` over a gap and allow `v1.1~` to be reoccupied by content checked only against `v1.0~`.

ADR-0013 therefore permits purge to release only a suffix of a major's minors. Together the rules establish one invariant: **admitted minors are always `{0..k}`, and the sequence grows and shrinks only at its end.** The hazard is decidable from local state, so purge enforces it as a precondition rather than documenting it as a risk.

An unstable Type Schema is exempt from the check and nothing else, since what ADR-0015 exempts is the enforced mode: a minor on a major-0 identifier is checked against nothing, while contiguity still holds and its major still opens at `v0.0~`.

### Breaking the chain deliberately: `force`

A registration may carry `force`, which **skips the cross-minor compatibility check for that candidate and does nothing else**. It is admissible only where that check would otherwise run — on a minor that has a predecessor in its major — and is refused on a major-only candidate, on the first minor of a major, and on a major-0 candidate, in each case because there is no check to skip and a flag that silently does nothing is a trap.

**The waiver is off unless a deployment turns it on.** One **global run-time configuration value, disabled by default**, governs `force`. It is read at process start, so a change requires restart.

When disabled:

* Dry Run and real submissions refuse `force` identically rather than ignore it;
* the named reason identifies deployment configuration, distinguishing it from a candidate with nothing to waive.

As with ADR-0013's purge control, silently ignoring a control would make it appear effective. Unlike disabled purge, however, a request field cannot disappear from the API surface. Availability is therefore learned through attempted use and operator documentation; no capability endpoint is added for one boolean.

Default-off follows from the flag's purpose. It is a rare deliberate act; repeated use means the contract likely belongs in major 0 or a new major. An exception callers can take unilaterally is not an exception.

With the flag off, every successive edge of a **stable** major is checked under BACKWARD without a per-edge waiver. `force` is a local relaxation opted into by an operator. Major 0 remains exempt from any enforced mode, so this claim concerns stable majors and the force axis, not the whole platform.

The value is **global and read at run time**; a build-time gate and a per-identifier-region variant were both considered and rejected — see *Sub-choices within the selected option*, below.

All three refusals below are decidable from the candidate's identifier alone, which is a consequence of contiguity rather than a coincidence: *first minor of a major* is `n == 0` once a major can only open at `M.0`, and the other two are the shape of the last segment. Under the *strictly higher* rule this was not so — `v1.7~` might have been the first member of an empty major — so admissibility of the flag would have depended on family state and could have changed between acceptance and execution.

It is safe in the sense that matters at admission: the identifier is new, so nothing references it, no domain row holds its Registry Reference, and no Instance conforms to it. Nobody is broken by the act. What is broken is the statement above — and only that, which is why `force` reaches the cross-minor edge and can never reach a revision of a major-only entity, where the same relaxation would break live consumers of a floating reference.

Everything else stands: derivation compatibility against the whole base chain, the dialect profile, the ADR-0015 quarantine, the identifier profile, the contiguity rule, and reference resolvability. `force` is not a general escape from admission and must not be described as one.

**A forced step is recorded and exposed; that is a condition of accepting it.** Otherwise two identifiers would look like an ordinary minor succession while silently withdrawing the inferred guarantee — the failure ADR-0015 avoids by putting its marker in the identifier.

The fact is not derivable from retained data. It is stored on the revision beside `gts_spec_version` and `gts_impl_version`, which likewise record how a verdict was reached — here, that there was none. DESIGN §3.3 exposes it through the `provenance` projection.

Two consequences of it being per-entity are stated rather than mechanised. The safe-upgrade statement holds across a run of minors **only if no member of that run was forced**, and the interval to inspect is precise: the flag records the edge *entering* a minor, so a move from `s` to `t` is established only if none of `s+1 … t` carries it. A forced edge entering `s` itself is irrelevant — the consumer is already there. And the registry offers no aggregate for that, because it would be a fold over facts the caller is already reading.

**Where `force` should not be reached for** is the case ADR-0015 already serves: a contract whose shape is not settled is a major-0 contract, where the relaxation is permanent, legible in the identifier, and quarantined. `force` is the opposite — a single break in a published major, visible only on request and quarantined by nothing — so reaching for it repeatedly is a sign the major should have been a v0 or should become a new major.

### What a managed version family is

A GTS Identifier can carry a version in more than one position. For a chained identifier such as `A.v1~B.v1~`, both `A` and `B` are versioned, so "the version family" needs a definition before the lifecycle rules below can be applied.

A managed version family is identified by the canonical GTS Identifier with the **whole version of its last segment removed** — the major and the minor, where a minor is present — and the trailing `~` of a Type Identifier normalized away, with every preceding segment held exactly as written.

```text
family(gts.acme.crm.customer.type.v1~)                 = (gts.acme.crm.customer.type)
family(gts.acme.crm.customer.type.v2~)                 = (gts.acme.crm.customer.type)          -- same family
family(gts.acme.crm.customer.type.v1.0~)               = (gts.acme.crm.customer.type)          -- same family
family(gts.acme.crm.customer.type.v1.7~)               = (gts.acme.crm.customer.type)          -- same family
family(gts.cf.core.events.type.v1~acme.crm.order.type.v1~)  = (gts.cf.core.events.type.v1~, acme.crm.order.type)
family(gts.cf.core.events.type.v1~acme.crm.order.type.v2~)  = (gts.cf.core.events.type.v1~, acme.crm.order.type)  -- same family
family(gts.cf.core.events.type.v2~acme.crm.order.type.v1~)  = (gts.cf.core.events.type.v2~, acme.crm.order.type)  -- DIFFERENT family
```

Stripping the minor as well as the major puts every minor of every major of one contract in one family, which is what the no-mixing rule above needs in order to be enforceable from the family row and what the pattern grain of a family enumeration is matched to. It also keeps the encoding **total**, which the previous formulation obtained by forbidding minors outright. A minor in a *preceding* segment survives verbatim, exactly as a major does — `family(gts.A.v1.2~B.v3~)` keeps `A.v1.2~` — because a difference anywhere but the last segment is a different family.

The consequences are deliberate:

* A major bump of a base type says nothing about anything derived from it. Types derived from the `v1` base keep their own lifecycle, which is required because their owners may be other gears or other tenants and this ADR already forbids publishing owner-visible contracts without owner intent.
* Adopting a new base major means admitting a **new logical entity in a new family**. `gts.cf.core.events.type.v2~acme.crm.order.type.v1~` does not succeed `gts.cf.core.events.type.v1~acme.crm.order.type.v1~`; the two are unrelated by version succession even though their identifiers look like siblings.
* A derived entity's status is independent of the status of the base it chains through; every non-deleted base remains a valid reference and derivation target.
* Version succession is therefore always a change in exactly one identifier segment — the last one. Types Registry never infers succession across a difference in any preceding segment.
* Succession within one major now carries a guarantee and succession across majors still carries none. A minor bump within a stable major is checked to be a safe upgrade unless `force` waives that one edge; a major bump is precisely how a change that is not one gets published, and a major-0 major offers the guarantee nowhere.
* **A minor is cheap on a leaf type and expensive on a base type, for the same reason a major is.** Adopting a new minor of a *referenced* target costs the dependent one content revision editing the reference string. Adopting a new minor of a *base* changes the dependent's own identifier, so it becomes a new logical entity in a new family with a new Registry Reference — and everything derived from it faces the same cascade, the one ADR-0015 records for graduating an unstable base. Authoring guidance must say so.
* Normalizing the kind marker away makes a family name **exclusive across kinds**: a derived Type Schema `gts.A~acme.crm.order.type.v1~` and a well-known registered Instance `gts.A~acme.crm.order.type.v1` map to the same family, and Types Registry admits whichever arrives first and refuses the other. This is a managed-profile restriction that GTS itself does not impose, alongside the restrictions on minor versions here and the prohibition on an explicit UUID tail in ADR-0001. It is adopted because the two identifiers differ by one character while denoting entirely unrelated things, nothing needs both — an Instance of that derived type is `gts.A~acme.crm.order.type.v1~<segment>`, not the colliding form — and because a family groups Version Successors, which are by definition of one kind. Keeping the marker in the key would instead let both families exist and force a shared owner on them, rejecting the second registrant over a family it may not be able to see.

### Lifecycle of family members

What happens to the members of a family over time — whether more than one may be usable at once, how a consumer learns which is newest, and whether deprecation exists — is decided by [ADR-0008](./0008-cpt-cf-types-registry-adr-managed-version-family-lifecycle.md). Only one property is stated here, because it is a property of identity rather than of lifecycle: admitting a compatible internal content revision preserves the logical entity's Lifecycle Status, since the revision is not a new member of the family.

### Reference and derivation rules

* A managed `$ref` to a GTS Type Schema is a floating reference to the current admitted revision of the exact logical entity it names — the entity whose identifier is that string, minor included where one is present.
* Floating stops at the minor, and in a minor-bearing major there is nothing left to float to: the entity a reference names is admitted once, so publishing `v1.3~` triggers no dependent revalidation, no effective-schema recomputation, and no cache invalidation anywhere outside its own entity.
* A reference to a minor that does not exist does not resolve, and a reference to `v1~` in a family that carries minors names nothing. There is no fallback to a neighbouring minor and no coverage of one identifier by another: adoption is written, never inferred.
* A reference remains valid for as long as its target is not deleted; no lifecycle metadata rewrites or invalidates a reference.
* Updating a referenced or base Type Schema does not rewrite the `$ref` string or the dependent GTS ID.
* Types Registry must revalidate affected registered dependency closure before activating the new base or referenced revision under ADR-0005. Because that closure is keyed on exact identifiers, it contains the dependents of the exact entity that took the revision and no others — and a minor-bearing entity never takes one, so publishing a minor revalidates nothing at all.
* Types Registry must not automatically create a new derived Type Schema when a base Type Schema changes, and in particular must not clone a derived type onto a new minor of its base.
* Tooling may generate a candidate derived definition for owner review, but registration and activation require an explicit owner operation.

### Exact resolution versus patterns

* Exact resolution is literal: it resolves only the entity whose canonical GTS ID equals the supplied identifier.
* Exact resolution must never use minor-version flexibility, version-membership expansion, or implicit pattern coverage. In particular there is no `latest`-minor resolution: `v1~` resolves `v1~` or nothing, never the highest `v1.x~`. Offering one would return the reference-pinning property that is the whole purpose of admitting a minor, and it is the same `latest` mode ADR-0008 declines for family members.
* Version-membership, hierarchy, and wildcard queries are separate operations and may return multiple exact identifiers or Registry References. None of them establishes compatibility between the members it returns; that is per edge and is read from provenance.
* A major-only ID used as a GTS pattern covers minor-versioned candidates according to `gts-rust` and GTS §10, and after this ADR that applies to managed candidates as well as externally managed ones. Pattern matching is therefore the one place where the minors of a major are collected under one expression, and it does not change exact resolution. Because a pattern-to-range compilation over the canonical identifier is only a pre-filter for a segment-wise matcher, the matcher's post-filter becomes load-bearing on a managed-only scan too, where the major-only profile had previously made the range exact.

### Externally managed entities

The managed identity policy does not rewrite authoritative external identities, and nothing in this ADR reaches an External Registry Source. *When a minor is admissible* governs admission into managed storage only; a source has always been free to serve minor-versioned identifiers and remains so.

* An External Registry Source may expose minor-versioned GTS IDs, major-only GTS IDs, or another source-owned revision convention.
* Types Registry preserves and returns the exact external GTS ID without storing or normalizing it.
* Types Registry does not synthesize a major-only GTS ID for `v1.0`, automatically advance references to `v1.1`, or claim that a source-owned immutable ID is mutable.
* Every live plugin response must provide an opaque `external_revision` and canonical `content_hash`.
* The same `external_revision` for one exact entity must always identify the same content and hash, and changed canonical content must produce a different revision.
* Types Registry does not require or persist an external versioning profile and does not interpret source revision ordering.
* The External Registry Source remains responsible for its evolution and compatibility rules; Types Registry applies only the federation response checks defined by ADR-0002 and makes no compatibility claim about source-owned content.
* The managed version-family definition is a property of Types Registry storage and is not imposed on an External Registry Source. How source lifecycle assertions map onto the platform model is decided by ADR-0008.

### What counts as an identifier conflict

A **major-only** managed GTS ID names a mutable logical entity, so two differing definitions under one identifier are not conflicting by that fact alone. What separates the two cases is revision lineage:

* sequential managed definitions admitted through ADR-0005 or ADR-0006 are revisions of one logical identity and retain the same Registry Reference. A later definition differing from an earlier one is evolution, not an identity collision;
* concurrent definitions that share no admitted revision lineage are conflicts;
* a stale registrar cannot replace the current revision with an older or divergent definition, which the per-candidate precondition of ADR-0012 enforces rather than this rule.

This concerns only which submissions conflict. Registry Reference representation and the exact forward and reverse identity guarantees are decided by ADR-0001 and are untouched by identity mutability: the reference names the logical entity, and every revision of it resolves under the same reference.

### Consequences

* Gear-owned domain rows require no reference migration for compatible evolution of a major-only entity, and require one per adopted minor in a minor-bearing major. That is what the owner of such a major chose.
* A Registry Reference identifies the logical entity, not the schema revision used at one historical moment.
* Resolution results and caches must include freshness metadata for a major-only entity, and must not assume a minor-bearing one is wholly static either: its authored content never changes, but its resolved form moves when a floating dependency of its own advances.
* Every framework and platform-gear type stays major-only through a compile-time lint over `gts.cf.*` in this repository, and through nothing in the registry. A minor admitted under that prefix by some other route is a well-formed entity the platform simply did not intend to author.
* An installation whose owners write no minor behaves as it did before this ADR, with two changes that reach it anyway: the family key strips the whole version, and the pattern post-filter is load-bearing on managed scans. Both are unobservable until a minor is admitted and both must be in place before one is.
* Minors accumulate `ACTIVE` members that dependents are pinned to while ADR-0008 defers authored deprecation. That gap is not created here, but it is reached sooner, and it is a further argument for closing PRD open question 1.
* `force` adds the one stored fact this decision needs — a boolean on the Type Schema revision — and the one place a reader learns of it, the `provenance` projection. Nothing else about the profile reaches storage, the deployment flag included: whether it was on when a revision was admitted follows from whether that revision carries the waiver.
* Because the flag is off by default, the reachable P1 behaviour of a stock deployment is BACKWARD with no per-edge exception at all. Turning it off again later does not retract waivers already applied — those majors keep their withdrawn guarantee, readable from provenance.
* Every family-level admission check except kind exclusivity becomes a keyed lookup rather than a scan, because contiguity fixes which single identifier answers each question. Admission needs no family snapshot to compare against at commit; it re-asks one existence question inside the transaction.
* **A vendor whose existing catalogue has gaps, or does not start at `.0`, must renumber to register it as managed.** That cost falls on the actor ADR-0011 directs toward managed registration, and it is the price of the property above. Two exits remain and both are already described: a new major, or an External Registry Source, which serves a self-contained universe under no platform numbering rule at all.
* **Relaxing this later is not additive, unlike every other narrowing of the managed profile.** Permitting gaps would return the baseline to a function of family state and would require the commit-time snapshot-and-recheck protocol that contiguity exists to avoid. The narrowing is cheap to adopt now and expensive to reverse, and that asymmetry is accepted deliberately rather than overlooked.
* The platform must adapt or wrap `gts-rust` behavior that assumes versioned IDs are append-only and resolved schemas can be cached forever.
* Type Schema and Instance revisions become correctness mechanisms rather than public GTS version components.
* Keying a family on the last identifier segment means version succession never crosses a derivation chain. Adopting a new base major produces an entity in a different family, so a derivation chain can hold simultaneously active entities that look like version siblings. Diagnostics, discovery, and documentation must present the family key rather than let readers infer succession from identifier similarity.
* Managed and external entities expose different revision ownership: Types Registry owns managed revision history, while a Registry Source Plugin supplies live opaque revisions for external entities.
* Existing managed minor-versioned entities, if any, require a separately planned migration and coexistence policy; this ADR governs new registration and does not silently rename existing identities.

### Confirmation

This decision is confirmed when:

* managed registration admits a Type Schema identifier carrying a minor under any prefix, `gts.cf.*` included, while an architecture lint rejects a minor in any `gts.cf.*` identifier declared in this repository at compile time;
* registering `v1.0~` into a family whose existing member is `v1~` is rejected and so is the reverse, concurrently as well as sequentially;
* exact resolution is tested separately from pattern and version-membership resolution, and `v1~` resolves nothing in a family whose members are `v1.0~` and `v1.1~`;
* a compatible managed update preserves the GTS ID and Registry Reference while changing the current internal revision;
* an incompatible update under the same managed GTS ID is rejected and requires a new major identity, and an unforced incompatible minor is rejected identically;
* `v1.1~` is checked against the definition of `v1.0~`, and an instance valid under `v1.0~` validates against the highest minor of that major where no edge in between was forced;
* a content revision of any minor-bearing entity is rejected, whether or not it is the highest minor of its major, while a revision of a major-only entity is admitted normally;
* a major opens only at `M.0`: `v1.1~` into an empty major 1 is rejected, and `v1.0~` is admitted;
* `v1.2~` is rejected while only `v1.0~` is admitted and succeeds once `v1.1~` is, with the first refusal reported as retryable rather than terminal;
* `v1.2~` is admitted while `v1.1~` is `DELETED`, and its check runs against `v1.1~`'s retained definition rather than against `v1.0~`;
* admitting `v1.1~` while `v1.2~` exists is rejected — as an occupied identifier, since contiguity makes that state unreachable — and `v1.1~` stays unavailable after `v1.2~` is deleted;
* **`v1.1~` and `v1.2~` submitted concurrently into a major whose highest minor is `v1.0~` never both commit unchecked**: `v1.2~` either fails retryably because its predecessor is absent, or is checked against `v1.1~`. The test asserts the ascending interleaving specifically, since the descending one was already refused under the superseded rule;
* a predecessor deleted and purged during validation of its successor causes the successor to fail rather than commit over a gap, proving the existence question is re-asked inside the commit transaction;
* two minors of one major submitted in one batch are admitted in ascending order, and the higher one is blocked rather than admitted when the lower one fails;
* a major-only `v1~` and a minor-bearing `v2.0~` coexist in one family, while `v1~` and `v1.0~` do not;
* `force` is refused in a deployment that has not enabled it, on a Dry Run identically to a real submission, with a reason naming the deployment configuration and distinguishable from the refusal of a candidate that has nothing to waive; with it enabled, `force` admits a `v1.1~` that is not backward compatible with `v1.0~`, and is itself rejected on a major-only candidate, on the first minor of a major, and on a major-0 candidate; a forced candidate that violates its base chain, its dialect, or the ADR-0015 quarantine is still rejected;
* a forced admission is recorded and returned through the `provenance` projection, and an unforced one is distinguishable from it;
* admitting a minor triggers no revalidation of any dependent of the preceding minor, and moves no `resolution_fingerprint` outside its own entity;
* a managed registered Instance identifier carrying a minor in its last segment is rejected, while an Instance of a minor-versioned Type Schema is admitted;
* the family key strips the whole version of the last segment, so `v1~`, `v1.0~`, `v1.7~`, and `v2~` resolve to one family while `A.v1.0~B.v1~` and `A.v1.1~B.v1~` resolve to different ones;
* a derived Type Schema and a well-known registered Instance whose identifiers differ only by the trailing `~` resolve to one family, and the second of them to be registered is refused whatever the order of arrival and whatever their owners;
* admitting an internal content revision changes no Lifecycle Status;
* reference validation accepts every visible and tenant-available non-deleted target, whatever its Lifecycle Status;
* dependent schemas and references are revalidated without automatic ID or `$ref` rewriting;
* external minor-versioned identities are resolved live without normalization or synthetic managed IDs;
* plugin contract tests reject a source that returns different canonical content or content hashes for the same external revision, without requiring Types Registry to persist external revision history;
* tests distinguish a valid sequential revision of one identity from a divergent definition that shares no admitted revision lineage with it.

## Pros and Cons of the Options

### Mandatory minor version, immutable entity

Backward-compatible changes create a new minor GTS ID; incompatible changes create a new major GTS ID.

* Good, because every GTS ID identifies one immutable definition.
* Good, because references and resolved schemas are reproducible and simple to cache.
* Good, because it follows the current `gts-rust` append-only version assumption.
* Bad, because compatible changes create new identities, Registry References, query results, and migration choices.
* Bad, because derived types and references remain pinned and require explicit owner-driven successors when adoption is desired.

### Mandatory minor version, mutable entity

* Good, because a family retains explicit minor labels.
* Bad, because the label no longer identifies immutable content.
* Bad, because there is no clear rule for when to increment the minor version.
* Bad, because it combines floating references with version-shaped identifiers and provides the weakest mental model.

### No minor version, immutable entity

Any content change creates a new major GTS ID.

* Good, because identity and content remain immutable.
* Bad, because compatible and incompatible changes both cause major-version churn.
* Bad, because compatibility can still be computed but is not reflected usefully in the public identity model.

### No minor version, mutable logical entity, platform-wide

Compatible changes update the current definition without changing the GTS ID; incompatible changes create a new major GTS ID. No identifier anywhere carries a minor.

* Good, because stored Registry References and exact `$ref` strings remain stable across compatible evolution.
* Good, because consumers see a major-version channel instead of a sequence of compatible public identities.
* Good, because the Types Registry compatibility and dependency engine owns evolution complexity centrally.
* Good, because one profile means a consumer never has to establish which one a given identifier is under.
* Bad, because references become floating with respect to internal revisions.
* Bad, because caches, validation, dependency closure, concurrency, and diagnostics require revision-aware behavior.
* Bad, because it intentionally differs from the current `gts-rust` assumption that versioned IDs are immutable and safely cacheable forever.
* Bad, because an owner has no way to publish a compatible successor without applying it to every dependent at once. A new major expresses non-adoption but discards the compatibility statement, so the two properties cannot be had together — which is the gap this ADR now closes.
* Bad, because a vendor arriving with a minor-versioned catalogue and an obligation to register it as managed under ADR-0011 has to flatten it, and the flattening is not information-preserving.

### Per-major shape: mutable major-only, immutable minors

The selected option. It absorbs an entry this list used to carry separately, *Two family-level evolution modes*, whose `PINNED_MINOR` half is this same design; the two were distinguished only by presentation. What this option drops is the mode itself — no flag is selected and none is stored, since the shape is read off the first member's identifier — and its grain, which was the family rather than the major.

That entry was faulted for producing "two admission, compatibility, and lifecycle models inside one registry", and the fault does not survive the merge: there is one model, a logical entity with retained revisions, of which a minor-bearing entity is the degenerate case with exactly one.

* Good, because the two properties that could not previously be had together — a successor checked to be a safe upgrade, and a successor nobody adopts implicitly — are both available to the owner who wants them.
* Good, because a minor-bearing entity is admitted once and never revised, so there is a single comparison baseline, no dependent revalidation across a minor boundary, and no rule reserving revisions to the newest member.
* Good, because it needs no policy state: the shape is in the identifier, the per-major rules are keyed lookups under the lock the family row already takes for ownership, and nothing an operator can edit changes what an admitted identifier means.
* Good, because contiguity makes the comparison baseline a function of the candidate's identifier rather than of family state, so concurrent admission cannot produce an unchecked step and no commit-time snapshot protocol is needed.
* Good, because the platform's own contracts keep one profile through a lint over their source rather than through a registry rule, so no vendor's prefix is special-cased at admission.
* Bad, because two profiles now exist and a reader must look at an identifier to know which — mitigated only by the minor being visible there, as ADR-0015 requires of the stability marker for the same reason.
* Bad, because **the shape of a major is an irreversible decision taken at its first admission**, before the consumers and derived types that would inform it exist. This is the strongest objection and it is not answered, only bounded: the grain is a major rather than a family, so the next major may choose differently.
* Bad, because every compatible change in a minor-bearing major costs a minor and a re-point for each dependent that wants it, where a major-only entity would have absorbed it in place. That is the trade, not a defect, but it is the reason major-only stays the recommendation.
* Bad, because publishing a minor of a widely derived **base** cascades new families and new Registry References down everything derived from it.
* Bad, because minors accumulate `ACTIVE` members with dependents pinned to them while ADR-0008 defers the deprecation signal that would let an owner discourage the old ones.
* Bad, because `force` puts a hole in the safe-upgrade statement that is visible only to a caller who selects provenance, where ADR-0015's comparable relaxation is visible in the identifier itself — mitigated by the flag being off by default, so the hole exists only where an operator opened it.
* Bad, because contiguity forbids gaps and reserved numbers, so a vendor catalogue that has either must be renumbered on the way into managed storage and its identifiers stop matching its own upstream.
* Bad, because that narrowing is the one clause of the managed profile that cannot be relaxed additively later: permitting gaps would bring back a state-dependent baseline and the commit-time recheck protocol with it.

### Sub-choices within the selected option

Three narrower alternatives were considered while shaping the option above and are recorded here rather than in *Decision Outcome*, which states what was chosen.

**Deployment configuration governing where a minor may be written.** This would let operators prevent their authors from using minors, at the cost of:

* a third prefix-policy system beside Source Claims and grants;
* the only managed identifier rule not readable from the identifier itself;
* installation-specific answers to whether an identifier is well formed;
* no retained record of the configuration under which a family was created.

ADR-0015 rejected the same trade for the adjacent marker because review, not admission, owns that control. A minor is an even weaker case: it *narrows* what an owner may do to consumers rather than widening it.

**A build-time gate for `force` instead of a run-time one.** Purge and `force` need different guards. Purge can cause stored rows to rebind after it releases an identifier, so making the capability absent from the artefact is valuable. `force` only withdraws a compatibility statement on a new identifier; the admission itself breaks nobody.

An auditor needs to know *whether an edge was forced*, not whether forcing was possible. `compat_forced` records that permanently per entity, regardless of the current flag. Configuration governs the future; provenance records the past.

A build flag would add an unnecessary capability claim, make a legitimate rare operation require redeployment, and turn a deployment choice into a product choice because one binary serves many deployments.

**A per-identifier-region `force` policy instead of a global one.** It would introduce:

* a third prefix-policy system beside Source Claims and grants;
* an identifier-profile verdict dependent on installation state and therefore unreadable from `gts.acme.crm.order.type.v1.3~` itself;
* historical ambiguity because overlapping regions change, while `compat_forced` records only whether the waiver occurred.

The plausible carve-out — forbidding `force` on platform contracts — is unnecessary. Repository lint keeps `gts.cf.*` major-only, and major-only candidates refuse `force`.

If regional control gains a consumer, it belongs in **authorization**: a PDP `force-register` action over the candidate identifier can reuse grant pattern matching, precedence, and audit. That addition does not require building a separate configuration policy now.

**The *strictly higher* ordering rule instead of contiguity.** *Strictly higher than every admitted minor* orders sequential admission but fails under concurrency:

1. With `v1.0` admitted, concurrent `v1.1` and `v1.2` both select `v1.0` as their baseline outside the transaction.
2. `v1.1` commits.
3. `v1.2` now satisfies *strictly higher* yet commits without being checked against `v1.1`.

The asymmetry makes this dangerous: the reverse interleaving visibly refuses `v1.2`, while the ascending one looks valid but leaves `v1.1~ ≤ v1.2~` unestablished.

The weaker rule *either `n-1` is admitted or the major is empty* has the same race. Concurrent `v1.5` and `v1.7` in an empty major can both qualify as first, be checked against nothing, and commit unrelated. Contiguity removes both races by construction; either alternative would need a commit-time snapshot protocol.

## More Information

### Relationship to the GTS specification

Two clauses of this decision narrow or relax what the specification describes, and each sits inside latitude the specification grants explicitly.

**Admitting minors, and constraining how they are numbered.** GTS §2.1 makes the minor optional. §4.2 allows a successive definition either to use a new MINOR or replace content under the same identifier. It leaves that choice to implementations and does not define the relationship between `v1~` and `v1.0~`, `v1.1~`.

This ADR defines the managed profile: the shapes never mix within a major, and minors are contiguous from `M.0`. This narrows the grammar just as ADR-0001 forbids an explicit UUID tail and ADR-0014 pins the dialect — to keep a platform guarantee decidable.

**The `force` waiver is a platform extension, and this ADR does not claim the specification authorizes it.** The honest position is a boundary rather than a licence, and it is drawn here so that nobody has to infer it.

The specification clearly leaves *which* mode a registry enforces and *how* it publishes successors open: §6 item 6 makes both implementation-defined, and §4.3 says the enforced mode is out of scope and may vary by namespace.

It does not clearly authorize declaring BACKWARD and then skipping it for one candidate. §5.3 requires a production registry to validate successive definitions and reject incompatible changes under its declared mode. Although framed as a capability — which this registry has and exercises by default — treating it as authorization for per-edge exceptions would be a stretch this ADR declines.

**So the limitation is stated instead.** A forced edge never establishes `Valid(v1.n-1) ⊆ Valid(v1.n)`, and the GTS minor-evolution type-safety guarantee does not hold across it. A product or deployment exposing `force` therefore does not claim unqualified §5.3 conformance; client restraint is not a server property.

A deployment requiring that claim leaves the flag **disabled, its default**. The server then validates every successor and refuses `force` instead of relying on callers not to use it. The platform still offers the waiver because a recorded, bounded exception for an unpublished successor can be safer than changing an identifier consumers already persisted.

Everything else remains unaffected: derivation compatibility under §4.1 and OP#12, trait validation under OP#13, and identifier and reference validity. `force` reaches only one relation and one edge.

It differs from ADR-0015 in exactly this respect and the difference is worth keeping visible. There a whole major declares no enforced mode, which §4.3 and §6 do plainly permit; here an enforced major has one unestablished step. If the specification is ever asked to accommodate this, a recorded per-edge waiver is the shape to propose.

Two properties keep the withdrawal legible rather than silent, and both are conditions on which it was accepted. The waiver is refused wherever there is no such check to skip, so it can never be present without meaning something. And it is recorded on the admitted revision and readable through provenance, so a consumer can establish for any run of minors whether the safe-upgrade statement holds across it — which is why this ADR states that statement with the proviso rather than unconditionally.

### Industry Practice

* [Google AIP-185](https://google.aip.dev/185) requires Google APIs to expose a major version such as `v1`, not minor or patch versions, and updates the major channel in place with compatible functionality.
* [Kubernetes API deprecation policy](https://kubernetes.io/docs/reference/using-api/deprecation-policy/) preserves API elements and significant behavior within an existing API version and introduces a new version for incompatible evolution.
* [GitHub REST API breaking-change policy](https://docs.github.com/en/rest/about-the-rest-api/breaking-changes) applies additive changes to supported API versions and places breaking changes in a new API version.
* [Confluent Schema Registry](https://docs.confluent.io/platform/current/schema-registry/fundamentals/schema-evolution.html) represents the alternative immutable-history model: a stable subject owns monotonically increasing schema versions checked under a compatibility policy.
* [Kubernetes CustomResourceDefinition versioning](https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definition-versioning/) is the closest analogue: several versions of one resource are served concurrently, a client names the version it wants, and publishing a new one adopts nobody. Nothing there floats a client onto a successor.

The managed model follows major-version API channels externally while ADR-0005 retains immutable internal schema revisions in the style of schema registries. A minor adds, for the owners who want it, the property those API-channel policies obtain by other means: a successor a consumer must name before it is bound by it.

## Traceability

- **PRD**: [../PRD.md](../PRD.md)
- **DESIGN**: [../DESIGN.md](../DESIGN.md)
- **ADR-0001**: [0001-cpt-cf-types-registry-adr-storage-identity-query-model.md](./0001-cpt-cf-types-registry-adr-storage-identity-query-model.md)
- **ADR-0002**: [0002-cpt-cf-types-registry-adr-external-source-live-delegation.md](./0002-cpt-cf-types-registry-adr-external-source-live-delegation.md)
- **ADR-0003**: [0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md](./0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md)
- **ADR-0005**: [0005-cpt-cf-types-registry-adr-type-schema-revisions.md](./0005-cpt-cf-types-registry-adr-type-schema-revisions.md)
- **ADR-0006**: [0006-cpt-cf-types-registry-adr-registered-instance-revisions.md](./0006-cpt-cf-types-registry-adr-registered-instance-revisions.md)
- **ADR-0007**: [0007-cpt-cf-types-registry-adr-federated-source-routing-query.md](./0007-cpt-cf-types-registry-adr-federated-source-routing-query.md)
- **ADR-0008**: [0008-cpt-cf-types-registry-adr-managed-version-family-lifecycle.md](./0008-cpt-cf-types-registry-adr-managed-version-family-lifecycle.md) — permits several members of a family to be `ACTIVE` at once, which is what lets the minors of a major coexist, and defers the deprecation signal that would let an owner discourage a superseded one.
- **ADR-0011**: [0011-cpt-cf-types-registry-adr-managed-external-boundary.md](./0011-cpt-cf-types-registry-adr-managed-external-boundary.md) — directs a vendor building on a platform contract to register as managed, which is what brings a minor-versioned vendor catalogue into the managed profile at all.
- **ADR-0013**: [0013-cpt-cf-types-registry-adr-platform-purge-of-deleted-entities.md](./0013-cpt-cf-types-registry-adr-platform-purge-of-deleted-entities.md) — releases identifiers, and therefore carries the one precondition that keeps a released minor number from reoccupying a point in the sequence this ADR orders.
- **ADR-0015**: [0015-cpt-cf-types-registry-adr-major-zero-unstable-profile.md](./0015-cpt-cf-types-registry-adr-major-zero-unstable-profile.md) — the precedent for carrying a profile in the identifier rather than in stored policy, the reason a minor is refused on a registered Instance identifier, and the source of the base-graduation cascade this ADR inherits for minor-versioned bases.

This decision directly addresses:

* `cpt-cf-types-registry-fr-minor-version-profile` - is the sole source of that requirement: when a minor is admissible, what it means, the per-major no-mixing and contiguity rules, the immutability of a minor-bearing entity, the `force` waiver and its record, and the lint that keeps the platform's own contracts major-only.
* `cpt-cf-types-registry-fr-gts-validation` - defines the platform profile for managed GTS version semantics, including when a minor version is admissible and where it is refused outright.
* `cpt-cf-types-registry-fr-validate-schema-compat` - maps compatible managed changes to in-place revisions or to a checked minor successor, and incompatible changes to a new major identity.
* `cpt-cf-types-registry-fr-id-resolution` - separates exact identity resolution from pattern and version-membership expansion, and refuses a `latest`-minor resolution mode.
* `cpt-cf-types-registry-fr-ref-tracking` - makes dependent revalidation mandatory when a floating managed reference changes revision, and bounds the revalidated set to the dependents of the exact minor that moved.
* `cpt-cf-types-registry-fr-lifecycle` - defines the version family that lifecycle transitions operate on; the transitions themselves are decided by ADR-0008.
* `cpt-cf-types-registry-fr-register-schemas`, `cpt-cf-types-registry-fr-register-instances` - restrict the admissible identifier profile of a managed candidate of each kind.
* `cpt-cf-types-registry-fr-type-query-assistance` - makes a pattern the one expression that collects the minors of a major — membership, not compatibility — and makes the matcher post-filter load-bearing on managed scans.
* `cpt-cf-types-registry-fr-externally-managed-entities` - preserves authoritative external minor and revision semantics, untouched by the managed profile.
