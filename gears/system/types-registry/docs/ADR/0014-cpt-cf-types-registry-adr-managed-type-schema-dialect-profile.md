---
status: accepted
date: 2026-07-30
decision-makers: Constructor Fabric Steering Committee
---

# Managed Type Schema JSON Schema Dialect Profile

**ID**: `cpt-cf-types-registry-adr-managed-type-schema-dialect-profile`

## Table of Contents

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Scope](#scope)
- [Terminology](#terminology)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [The P1 admissible set](#the-p1-admissible-set)
  - [The dialect is pinned at initial admission](#the-dialect-is-pinned-at-initial-admission)
  - [What P2 widens to](#what-p2-widens-to)
  - [Why mixing breaks derivation](#why-mixing-breaks-derivation)
  - [Why mixing breaks evolution](#why-mixing-breaks-evolution)
  - [The dialect is not persisted](#the-dialect-is-not-persisted)
  - [Externally Managed Entities are excluded](#externally-managed-entities-are-excluded)
  - [What an owner does instead](#what-an-owner-does-instead)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Accept every dialect and resolve mixing case by case](#accept-every-dialect-and-resolve-mixing-case-by-case)
  - [Accept every dialect and require closure uniformity immediately](#accept-every-dialect-and-require-closure-uniformity-immediately)
  - [Accept Draft-07 only, dialect pinned at initial admission](#accept-draft-07-only-dialect-pinned-at-initial-admission)
  - [Accept Draft 2020-12 only](#accept-draft-2020-12-only)
- [More Information](#more-information)
  - [Relationship to the GTS specification](#relationship-to-the-gts-specification)
  - [Industry Practice](#industry-practice)
- [Traceability](#traceability)

<!-- /toc -->

## Context and Problem Statement

A GTS Type Schema is a JSON Schema document, and GTS is deliberately dialect-agnostic: specification §11.0 states that the dialect of a concrete Type Schema is set by its own `$schema` URI, that Draft-07, Draft 2019-09, and Draft 2020-12 are all admissible, and that GTS publishes no meta-schema of its own. The specification also defines every compatibility relation in terms of accepted-instance sets evaluated **under the dialect declared by each schema** (§4.3).

Those two statements together leave a gap that the specification does not close. A GTS type is almost never one document: a derived type resolves its base chain, and a Type Schema resolves `$ref` targets, including references inside `x-gts-traits-schema`. When the documents in one resolution closure declare different dialects, the specification says nothing about which dialect governs the composed result, and it says nothing about whether a successive definition of one type identity may declare a different dialect from the definition it replaces.

Types Registry cannot leave that unanswered, because both of its central relations are defined over accepted-instance sets. ADR-0003 enforces `Valid(current) ⊆ Valid(candidate)` and derives its soundness from the transitivity of set inclusion; `cpt-cf-types-registry-fr-validate-type-derivation` enforces `Valid(derived) ⊆ Valid(base)` over the whole chain. Neither relation is well-posed when the two sets are defined under different validation semantics.

The platform GTS implementation resolves this gap silently rather than leaving it open:

* `resolve_schema_refs` removes `$id` and `$schema` from a referenced type inlined below the root;
* effective trait composition takes the dialect from the **leaf** and re-injects it after stripping ancestor `$schema` values.

Consequently, **the referring document's dialect governs the whole resolved closure and every base dialect is discarded.** Mixing does not fail; it reinterprets content.

That reinterpretation is unsafe in both directions, because JSON Schema ignores keywords it does not recognise rather than rejecting them. Constraints therefore disappear without an error.

This ADR decides the dialect profile of Managed Entities.

## Scope

This ADR decides:

* which JSON Schema dialects a managed GTS Type Schema may declare in P1;
* whether the declared dialect may change between successive revisions of one logical entity;
* which dialect governs a resolution closure, and what P2 must require when the admissible set widens;
* the accepted spelling of `$schema` and the treatment of a `$schema` below the document root;
* whether the declared dialect is persisted as registry state;
* whether any of this applies to Externally Managed Entities.

This ADR does not decide the enforced compatibility mode or the comparison baseline (ADR-0003), managed identity semantics (ADR-0001, ADR-0004), revision identity and retention (ADR-0005, ADR-0006), or the write-path protocol through which the check runs (ADR-0012). It does not decide the schedule on which P2 widens the admissible set.

## Terminology

| Term | Meaning |
|---|---|
| Dialect | The JSON Schema draft a document declares through its top-level `$schema` URI. |
| Resolution closure | The set of documents inlined to produce a Type Schema's effective form: every base in its `$id` chain plus every external `$ref` target reachable from its content, including targets referenced from inside `x-gts-traits-schema`. |
| Leaf | The document being resolved — the one whose `$id` names the entity, as distinct from the bases and `$ref` targets composed into it. |
| Dependency closure | The availability-blocking relation of ADR-0010. For a Type Schema it follows the resolution-bearing base and `$ref` relationships; `x-gts-ref` targets are instance-value constraints and belong to neither closure. |

## Decision Drivers

* Both platform compatibility relations are defined over accepted-instance sets, and a set is only defined once a dialect is fixed.
* ADR-0003's candidate-versus-baseline check is sound only because set inclusion is transitive, which requires all the sets in a revision chain to live in one universe.
* JSON Schema ignores unrecognised keywords, so a dialect mismatch removes constraints silently rather than failing.
* The platform GTS implementation already resolves mixing by discarding every non-leaf dialect. An authoritative admission decision must not rest on behaviour that was chosen for reference inlining rather than for compatibility semantics.
* Types Registry does not implement GTS. A question the specification leaves open is a change request against the specification, not something to answer locally with a platform-specific rule.
* The platform's own schema-generation toolchain emits Draft-07 exclusively, so a Draft-07 profile costs nothing today.
* A restriction adopted while the admissible set has one member is free; the same restriction adopted later must be reconciled with live data.

## Considered Options

* **Accept every dialect the implementation supports and resolve mixing case by case.**
* **Accept every dialect and require dialect uniformity across the resolution closure immediately.**
* **Accept Draft-07 only, with the dialect pinned at initial admission.**
* **Accept Draft 2020-12 only.**

## Decision Outcome

Chosen option: **a managed GTS Type Schema MUST declare Draft-07 in P1, and the declared dialect is pinned at initial admission and immutable across the logical entity's revision chain.**

### The P1 admissible set

A managed Type Schema candidate **MUST** declare a top-level `$schema` naming JSON Schema Draft-07. Any other dialect is rejected at admission with a diagnostic naming the admissible value.

`$schema` **MUST** be present. Under specification §11.1 Rule A a document is a schema if and only if it carries a top-level `$schema`, so an absent `$schema` means the submitted document is not a Type Schema at all and is rejected as such. Types Registry **MUST NOT** rely on a validator's default-dialect fallback to supply a missing value, because that default is a property of the validator rather than of the contract.

The accepted spelling is a closed list. The canonical form is `http://json-schema.org/draft-07/schema#`, which is what the platform toolchain emits. Types Registry **MUST** additionally accept the same URI without the trailing `#` and under the `https` scheme, normalising all three to the canonical form. There is exactly one Draft-07 meta-schema, so those differences carry no semantic content, and rejecting a registration over a trailing `#` would be an authoring obstacle with no corresponding guarantee.

A `$schema` **below** the document root **MUST** be absent, or equal to the root's value. This closes the one way mixing could otherwise enter a single document: post-Draft-07 dialects permit an embedded resource to switch dialect, and the resolver strips a nested `$schema` when inlining, so a divergent nested value would be discarded without a diagnostic — the exact failure mode this ADR exists to prevent.

The restriction applies to Type Schemas. A registered Instance is a value document with no `$schema` and therefore has no dialect of its own; it is validated against the current revision of its conforming Type Schema, whose dialect governs.

### The dialect is pinned at initial admission

The dialect recorded by the initial admitted revision of a logical entity is the dialect of that entity. A content revision **MUST NOT** change it.

**The pin is on the major, not the identifier**, where ADR-0004 minors are used. The first revision of a new minor is checked against the current revision of the preceding minor, so that edge compares accepted-instance sets just like two revisions of a major-only entity.

Different dialects make that inclusion ill-posed, and a successor changing only `$schema` can still change semantics. Every minor must therefore declare the dialect under which its major was admitted.

That is also why a new minor is **not** a route out of a dialect. The remedy for an owner who needs a different one is the same as for any change the enforced mode cannot admit: a new major identity, under ADR-0004. A new major starts a chain of its own, which is precisely what makes a dialect change well-posed there and ill-posed anywhere else.

In P1 all of this is vacuous, because the admissible set has one member. It is stated now because it is the second question the specification leaves open, and answering it while it is still free is materially cheaper than answering it in P2 against live revision chains.

### What P2 widens to

When the admissible set widens, the rule that replaces the P1 restriction is **dialect uniformity across the resolution closure**: every document inlined to produce an entity's effective form — each base in its `$id` chain and each `$ref` target reachable from its content, including targets referenced from inside `x-gts-traits-schema` — **MUST** declare the same dialect as the leaf.

Uniformity is decidable entirely from local state. ADR-0011 closes the managed–external boundary, so a managed resolution closure contains only Managed Entities, and admission already loads every closure member in order to resolve references and compute the effective schema. The check is therefore a comparison over documents already in hand.

`x-gts-ref` targets are deliberately **not** subject to uniformity. An `x-gts-ref` is a constraint on an instance value, not a schema dependency to inline; the platform implementation excludes it from reference resolution for exactly that reason. It is also non-blocking under ADR-0010: the named entity contributes no content to the constraining schema's semantic contract, so it belongs to neither closure and cannot be reinterpreted under another dialect.

P1 is the degenerate case of this rule rather than a separate regime: when only one dialect is admissible, every closure is trivially uniform. That is what makes the widening additive.

### Why mixing breaks derivation

Type Derivation Compatibility requires `Valid(derived) ⊆ Valid(base)` for every base in the chain. Under mixing, the two sides are not evaluated under one semantics, and because the resolver discards every non-leaf `$schema`, the base is evaluated under the *derived* type's dialect. Four concrete failures follow.

**A downgrade silently drops base constraints.** A base closed through `unevaluatedProperties: false` — Draft 2019-09 and later — reads as open to a Draft-07 derived type, because the keyword is unrecognised and therefore ignored. The derived type then passes derivation against constraints that were never enforced, and admission certifies substitutability that does not hold. `dependentRequired` and `dependentSchemas`, split out of Draft-07's `dependencies`, behave the same way.

**An upgrade drops constraints too.** Draft-07 permits the array form of `items` for tuple validation; Draft 2020-12 moved that to `prefixItems`. A Draft-07 base reinterpreted under a Draft 2020-12 derived type loses the positional constraint. Mixing is unsafe in both directions.

**`$ref` itself changes meaning.** Draft-07 lets `$ref` replace its schema object and ignores every sibling; from Draft 2019-09 `$ref` is an applicator and siblings apply. So `{"$ref": base, "additionalProperties": false}` means "the base" under one dialect and "the base, closed" under the other, and the effective schema depends on which dialect governs the *referring* document. The specification's recommended `allOf: [{$ref: base}, {overlay}]` shape is robust because the `$ref` sits alone in its branch, which is why the hazard is latent in generated schemas rather than immediately visible.

**Content-model classification has no single answer for a mixed closure.** ADR-0003 makes the per-level open/closed/partial classification load-bearing and requires it to come from the resolved form, precisely because closure can be established through a keyword whose availability is dialect-dependent.

The trait system inherits all of this: the effective traits schema is `allOf`-composed along the `$id` chain under the leaf's dialect after each ancestor's is stripped, so an ancestor's post-Draft-07 trait constraint — including a `const` lock meant to fix a value across descendants — vanishes for a Draft-07 descendant and OP#13 completeness is checked against a weaker schema than its author wrote.

### Why mixing breaks evolution

**The relation becomes ill-posed.** If two revisions declare different dialects, `Valid(current) ⊆ Valid(candidate)` compares sets defined under different semantics. A revision byte-identical to its predecessor that changes only `$schema` can then be a genuine semantic change in either direction, including one that narrows and must be rejected — and a change with no visible content is exactly the kind that reaches production unexamined.

**It breaks the transitivity the baseline rests on.** Candidate-versus-current is sound because `Valid(rev_n) ⊆ Valid(rev_n+1)` composes to `Valid(rev_1) ⊆ Valid(rev_N)`, which holds only while all the sets live in one universe. This is structurally the same defect ADR-0003 identifies for a semantic change of the relation, whose handling that ADR defers while recording the data to detect it.

**It would add a second way for a chain to span two semantics, and a worse one.** ADR-0003's is platform-wide, rare, and detectable after the fact from the versions recorded on each revision. A dialect axis would be per-entity and author-initiated, arising per revision rather than per upgrade event, and undetectable in the same way — nor could it be repaired by comparing retained revisions, since each would declare its own dialect with nothing to normalise them to.

**The verdict would depend on unspecified library behaviour.** The document-level comparison entry point takes two documents and no dialect. Its behaviour on a mismatched pair is not part of its contract, and deriving an authoritative admission decision from it would violate `cpt-cf-types-registry-principle-fail-closed`.

### The dialect is not persisted

Types Registry **MUST NOT** store the declared dialect as a column of registry state. It is a top-level key of a document the registry already retains in full, so a stored copy would be a duplicate of a derivable fact, requiring an invariant to keep the two from diverging — which `cpt-cf-types-registry-principle-derive-not-store` prohibits.

The comparison with `gts_spec_version` and `gts_impl_version` does not carry: those record a property of the *checker* used at admission, which is nowhere in the content and cannot be recovered from it. The dialect is a property of the content.

Nor does persisting it buy anything on the P2 uniformity check. Admission necessarily loads every closure member to resolve references, so the dialect is available without a dedicated read. Should a post-P2 operational report need to partition entities by dialect, the column can be introduced at that point, when the value stops being constant and begins to carry information.

### Externally Managed Entities are excluded

None of this applies to an Externally Managed Entity. An External Registry Source owns its own evolution and derivation rules under ADR-0002, and the platform makes no compatibility claim about its content.

The exclusion is safe because of ADR-0011, and would not be safe without it. The closed boundary means no managed resolution closure can contain an external document, so an external entity's dialect can never reach a managed verdict. Were the boundary open in that direction — a managed schema permitted to reference an external one — an external dialect would enter managed resolution and this exclusion would be a hole.

Types Registry **MUST NOT** inspect an external document's dialect. Doing so would require parsing source-owned content on the live read path, which ADR-0011 rejects permanently for the cross-boundary reference check and rejects here for the same three reasons: content parsing on the read path, the platform reading source content to enforce a platform rule, and a documented limitation turned into an integration barrier.

### What an owner does instead

An owner who needs a post-Draft-07 keyword in P1 has two options and no third: express the constraint in Draft-07, or defer the type until the admissible set widens. Closure through `additionalProperties: false` plus `allOf` composition reproduces the closed-envelope shape of specification §4.4.1, which is what the platform toolchain generates, so the loss is narrower than the keyword list suggests.

A vendor holding existing Draft 2020-12 schemas that it wants registered as Managed Entities — the path ADR-0011 directs it to when it needs to build on a platform contract — must convert them. The conversion is usually mechanical (`$defs` to `definitions`, `prefixItems` to the array form of `items`) but is not guaranteed to be.

### Consequences

* Managed admission acquires one envelope check, placed with the other GTS validity checks and running before compatibility evaluation.
* The transitivity argument of ADR-0003 becomes airtight on the dialect axis, leaving exactly one way for a chain to span two semantics — a change to the relation itself, which is recorded and platform-wide.
* Content-model classification in P1 has one fewer input: closure can only be established through `additionalProperties` and `allOf`/`$ref` composition, never through `unevaluatedProperties`. The classification implementation still handles the keyword; managed content will not contain it.
* The keywords unavailable to managed Type Schemas in P1 are `$defs`, `unevaluatedProperties`, `unevaluatedItems`, `prefixItems`, `dependentRequired`, `dependentSchemas`, and `$dynamicRef`/`$dynamicAnchor`.
* No storage change. `database.sql` is unaffected.
* The restriction lands on the XaaS Vendor Architect, the actor ADR-0011 directs toward managed registration. That is an argument for not deferring the P2 widening indefinitely, not against the P1 profile.
* Two questions the GTS specification leaves open — which dialect governs a mixed closure, and whether a successive definition may change dialect — are avoided rather than answered locally. Both should be raised against the specification; the analysis in this ADR is the input for doing so.
* Widening in P2 is additive: existing chains are uniform by construction, so no P1 state has to be reinterpreted.

### Confirmation

This decision is confirmed when:

* managed admission rejects a Type Schema candidate declaring Draft 2019-09 or Draft 2020-12, with a diagnostic naming the admissible dialect;
* a candidate with no top-level `$schema` is rejected as not being a Type Schema, and no validator default supplies a dialect;
* the canonical Draft-07 URI, the same URI without a trailing `#`, and its `https` spelling are all accepted and normalised to the canonical form, while any other value is rejected;
* a candidate carrying a `$schema` below the document root is rejected unless its value equals the root's;
* a registered Instance is admitted with no dialect of its own and is validated against its conforming Type Schema's revision;
* no column of Types Registry storage holds a declared dialect;
* an externally managed entity declaring a non-Draft-07 dialect is resolved and returned without objection, and no federation response validation reads `$schema` from returned content;
* a test fixture pair that differs only in declared dialect is rejected at admission rather than being compared for compatibility, whether the pair is two revisions of one identifier or a new minor and the minor before it;
* the P2 uniformity rule is exercised ahead of P2 by a test proving that a closure whose members all declare Draft-07 is accepted and that the check reads the dialect from documents already loaded for reference resolution.

## Pros and Cons of the Options

### Accept every dialect and resolve mixing case by case

* Good, because it imposes no restriction on authors and needs no P2 follow-up.
* Bad, because the two relations the registry enforces are undefined for a mixed pair, so every "case" would be Types Registry inventing GTS semantics the specification does not define.
* Bad, because the resolver discards non-leaf dialects, so the failures are silent constraint loss rather than errors — the worst available failure mode for a check whose purpose is to certify substitutability.
* Bad, because it adds a per-entity, author-initiated way for a chain to span two semantics, alongside the platform-wide one that is at least recorded and detectable.

### Accept every dialect and require closure uniformity immediately

This is the honest alternative, and it is what P2 becomes.

* Good, because it admits Draft 2020-12 authors from the start and closes the mixing hazard properly rather than by narrowing the input.
* Good, because uniformity is decidable from local state under the closed boundary of ADR-0011.
* Bad, because it still has to answer whether a revision may change dialect, and whether an owner can move an existing closure to another dialect — questions with no demand behind them today.
* Bad, because nothing currently produces a non-Draft-07 managed Type Schema: the platform toolchain emits Draft-07 exclusively, so the whole mechanism would ship untested against a real case.
* Neutral, because adopting it later is additive, which is exactly why it can wait.

### Accept Draft-07 only, dialect pinned at initial admission

* Good, because it makes both relations well-posed with a single envelope check, and costs nothing against current content.
* Good, because it leaves one way for a chain to span two semantics and preserves the candidate-versus-baseline check.
* Good, because it answers the revision-mutability question while the answer is free, and makes the P2 rule a widening of a special case rather than a new regime.
* Bad, because it withholds post-Draft-07 keywords from managed authors, including `unevaluatedProperties` as a closure mechanism.
* Bad, because a vendor with existing Draft 2020-12 schemas must convert them or wait, and that cost falls on the actor ADR-0011 pushes toward managed registration.

### Accept Draft 2020-12 only

* Good, because it standardises on the current dialect and would need no widening later.
* Bad, because the platform toolchain, the specification's own examples, and every existing fixture are Draft-07, so it would reject the platform's own types on day one.
* Bad, because it buys a newer keyword set that no named requirement asks for, at the cost of an immediate migration of everything.

## More Information

### Relationship to the GTS specification

This ADR does not contradict the specification. §11.0 permits an implementation to accept a subset of dialects — it states which dialects are valid GTS Type Schemas, not that a registry must accept all of them — and §6 leaves publication policy to the implementation. Narrowing the profile is the same kind of latitude ADR-0004 uses to constrain how minor versions are numbered and ADR-0001 uses to forbid an explicit UUID tail.

What the specification genuinely does not define is which dialect governs a resolution closure whose members disagree, and whether a successive definition of one type identity may change dialect. Both are gaps rather than latitude, because §4.3 makes every compatibility verdict dialect-relative while §11.0 makes the dialect a per-document property. This ADR sidesteps them for P1; closing them properly belongs in the specification, and the derivation and evolution analyses above are the material for that request.

### Industry Practice

* [Confluent Schema Registry](https://docs.confluent.io/platform/current/schema-registry/fundamentals/serdes-develop/index.html) pins one schema format per subject and does not compare across formats. A format change is a new subject, not a new version — the same conclusion this ADR reaches by requiring a new major identity.
* [OpenAPI 3.1](https://spec.openapis.org/oas/v3.1.0#schema-object) resolved the equivalent problem by pinning a single dialect for the whole document rather than per schema object, having found per-object dialects unworkable in a composed document.
* [JSON Schema's own guidance on `$schema`](https://json-schema.org/understanding-json-schema/reference/schema) treats the dialect as a property of a schema resource and expects a referenced resource to declare its own; validators differ in how they handle a reference across dialects, which is why relying on that behaviour for an authoritative decision is unsound.

## Traceability

- **PRD**: [../PRD.md](../PRD.md)
- **DESIGN**: [../DESIGN.md](../DESIGN.md)
- **ADR-0001**: [0001-cpt-cf-types-registry-adr-storage-identity-query-model.md](./0001-cpt-cf-types-registry-adr-storage-identity-query-model.md)
- **ADR-0002**: [0002-cpt-cf-types-registry-adr-external-source-live-delegation.md](./0002-cpt-cf-types-registry-adr-external-source-live-delegation.md)
- **ADR-0003**: [0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md](./0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md)
- **ADR-0004**: [0004-cpt-cf-types-registry-adr-gts-minor-version-identity-evolution.md](./0004-cpt-cf-types-registry-adr-gts-minor-version-identity-evolution.md)
- **ADR-0005**: [0005-cpt-cf-types-registry-adr-type-schema-revisions.md](./0005-cpt-cf-types-registry-adr-type-schema-revisions.md)
- **ADR-0006**: [0006-cpt-cf-types-registry-adr-registered-instance-revisions.md](./0006-cpt-cf-types-registry-adr-registered-instance-revisions.md)
- **ADR-0010**: [0010-cpt-cf-types-registry-adr-tenant-availability-evaluation.md](./0010-cpt-cf-types-registry-adr-tenant-availability-evaluation.md)
- **ADR-0011**: [0011-cpt-cf-types-registry-adr-managed-external-boundary.md](./0011-cpt-cf-types-registry-adr-managed-external-boundary.md)
- **ADR-0012**: [0012-cpt-cf-types-registry-adr-write-path-admission-protocol.md](./0012-cpt-cf-types-registry-adr-write-path-admission-protocol.md)
- **GTS specification**: [Global Type System](https://github.com/globaltypesystem/gts-spec) §4.3, §4.4, §6, §11.0, §11.1

This decision directly addresses:

* `cpt-cf-types-registry-fr-gts-validation` - adds the declared dialect to the managed GTS envelope checked at admission, and excludes Externally Managed Entities from it.
* `cpt-cf-types-registry-fr-validate-schema-compat` - makes the comparison baseline well-posed by guaranteeing that both sides of every revision edge are evaluated under one dialect.
* `cpt-cf-types-registry-fr-validate-type-derivation` - guarantees that a base's constraints are evaluated under the dialect they were authored in rather than reinterpreted under the derived type's.
* `cpt-cf-types-registry-fr-register-schemas` - restricts the admissible content profile of a managed Type Schema candidate.
* `cpt-cf-types-registry-fr-externally-managed-entities` - confirms that the profile does not reach source-owned content and that its dialect is never inspected.
