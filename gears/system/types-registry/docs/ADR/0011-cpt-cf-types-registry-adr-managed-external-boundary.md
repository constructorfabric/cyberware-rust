---
status: accepted
date: 2026-07-29
decision-makers: Constructor Fabric Steering Committee
---

# The Managed–External Boundary

**ID**: `cpt-cf-types-registry-adr-managed-external-boundary`

## Table of Contents

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Scope](#scope)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [The persistence rule](#the-persistence-rule)
  - [No references across the boundary, in either direction](#no-references-across-the-boundary-in-either-direction)
  - [The external half of the rule is only half enforceable](#the-external-half-of-the-rule-is-only-half-enforceable)
  - [Source Claims are rooted single-segment patterns](#source-claims-are-rooted-single-segment-patterns)
  - [Deletion safety is entirely local](#deletion-safety-is-entirely-local)
  - [A retired Source Claim reserves its space until purge](#a-retired-source-claim-reserves-its-space-until-purge)
  - [What a vendor does instead](#what-a-vendor-does-instead)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Closed both ways](#closed-both-ways)
  - [Asymmetric, with registered external dependents](#asymmetric-with-registered-external-dependents)
  - [Open, with a provenance carve-out](#open-with-a-provenance-carve-out)
  - [Query every plugin for reverse impact at deletion time](#query-every-plugin-for-reverse-impact-at-deletion-time)
- [More Information](#more-information)
  - [Industry Practice](#industry-practice)
- [Traceability](#traceability)

<!-- /toc -->

## Context and Problem Statement

ADR-0002 puts Externally Managed Entities entirely under their source: Types Registry persists no external definitions, revisions, hashes, dependencies, lifecycle, mappings, caches, or tombstones. ADR-0007 routes to sources through non-overlapping Source Claims. Both describe a clean separation.

The separation stops being clean the moment an entity on one side of the boundary points at an entity on the other. Four consequences of that were left unresolved and now block DESIGN:

* a managed Type Schema that references an external one would have to record the external revision it validated against, which ADR-0005 requires and ADR-0002 forbids;
* a derived GTS Identifier is a chain, so a Source Claim written for a base can appear to capture types derived from it;
* deleting a Managed Entity must be safe against dependents that live in a source, which would otherwise make the deletion of a platform type depend on the health and honesty of every vendor plugin;
* retiring a plugin releases its claimed identifier space, after which the same identifier could be re-registered as managed and deterministic Registry References would silently rebind persisted domain data.

The common question underneath is what may cross the boundary, in which direction, and what Types Registry is permitted to persist about a crossing. The answer that suggests itself is asymmetric — nothing outward, dependents inward, with the inward edges registered by plugins as facts. This ADR does not take it, and the reason is recorded in full below: a registered edge trades dependence on a plugin's *uptime* for dependence on a plugin's *diligence*, and the second is not observable.

## Scope

This ADR decides:

* whether a Managed Entity may reference or derive from an Externally Managed Entity;
* whether an Externally Managed Entity may reference or derive from a Managed Entity;
* how a Source Claim matches a chained GTS Identifier;
* how deletion of a Managed Entity is made safe against dependents held in a source;
* what happens to a Source Claim when its plugin is retired;
* the persistence rule that governs all of the above.

This ADR does not decide the live delegation contract (ADR-0002), routing and pagination (ADR-0007), Registry Reference derivation (ADR-0001), or the plugin request and response types, which belong to the SDK design.

## Decision Drivers

* Every platform guarantee that Types Registry offers for a Managed Entity must be enforceable by Types Registry, not contingent on a source's cooperation.
* A source changes without notifying the platform. Any managed guarantee that presumes otherwise is a guarantee in name only.
* An authoritative decision — admission, deletion, availability — must not depend on the uptime **or the diligence** of a component the platform does not operate.
* Resolution of a Managed Entity must stay on the 10 ms managed-lookup budget, which a live plugin call inside that path would break.
* An invariant enforced from data a counterparty supplies is only as complete as that counterparty made it, and its incompleteness is undetectable.
* A write path granted to an out-of-process counterparty is a permanent surface: authentication, authorization, idempotency, withdrawal, and the operational consequences of getting any of them wrong.
* The rule against persisting external state exists to prevent a second source of truth, not to prevent Types Registry from knowing facts about its own entities — but a fact whose far end the platform cannot inspect is barely a fact it knows.

## Considered Options

For crossings of the boundary:

* **Closed both ways.** Neither direction may cross; the two identifier universes are disjoint by construction.
* **Asymmetric.** Forbid managed-to-external; permit external-to-managed, with the resulting dependency edges registered by plugins and stored by Types Registry.
* **Open with a provenance carve-out.** Permit managed-to-external and store the external revision inside the immutable managed revision as write-once provenance.

For dependents held in a source, given the asymmetric option:

* Plugins register their dependencies on managed identifiers; Types Registry stores the edges.
* Types Registry queries every plugin for reverse impact at deletion time.

## Decision Outcome

Chosen option: **the boundary is closed in both directions. The managed and externally managed identifier spaces are disjoint, and no reference or derivation crosses between them.**

A Managed Entity may not reference or derive from an Externally Managed Entity, and an Externally Managed Entity may not reference or derive from a Managed Entity. Admission rejects the first. The second is enforced only in part, and the part that is not enforced is stated explicitly below rather than left to be discovered: derivation across the boundary is structurally impossible, while a reference from inside an external schema document is representable by the source and undetectable by the platform.

### The persistence rule

> Types Registry persists no state whose authority belongs to a source.

That is the whole rule, with no exception. ADR-0002's enumeration of prohibited persistence stands in full: an external entity's definition, identifier, revision, content hash, lifecycle, tenant state, availability, dependencies, caches, tombstones, and Registry Reference mapping are source-authoritative and are never stored.

One plausible exception is an external identifier stored as a dependency-edge label. The rationale is sound: dependents of a Managed Entity look like a fact about that entity, not source state.

The exception is still unnecessary because a closed boundary leaves nothing observable to record:

* cross-boundary derivation is structurally impossible;
* an external document may still reference a managed identifier, but the platform cannot detect it because plugins have no write path and returned content is never parsed.

There is therefore no **registered or platform-observable** external dependent from which to obtain a label.

### No references across the boundary, in either direction

**Managed to external** fails on each of the platform's four guarantees independently, and this half of the decision is unchanged:

The four guarantees below concern resolution-bearing dependencies. `x-gts-ref` creates none, but the disjointness rule still covers its authority identifier: Managed admission checks current Source Claims, and claim activation reclassifies current Managed Type Schema content. Neither path stores a dependency.

* **Compatibility.** ADR-0004 makes a managed `$ref` float to the current revision, and ADR-0005 requires the affected dependency closure to be revalidated before a referenced revision becomes current. A source publishes a new revision without telling the platform, so the revalidation that makes floating references safe never runs. ADR-0003 already concluded that a managed schema referencing an external one inherits the weakest guarantee in its closure; with no per-revision monotonicity assertion, that is no guarantee.
* **Deletion safety.** A source may delete its entity unilaterally. The managed entity that references it breaks with no registry event and no opportunity to block.
* **Availability.** Under ADR-0010 a `$ref` target is an availability-blocking edge, so every availability evaluation for the managed entity would require a live plugin call. That places a network hop inside the managed resolution path and makes the managed lookup NFR contingent on plugin latency.
* **Provenance.** ADR-0005 requires each admitted revision to record the dependency revisions it was validated against. For an external dependency the concurrency purpose of that record is unachievable anyway — Types Registry cannot hold the target still — so the record would carry cost without the guarantee it exists to provide.

The conflict between ADR-0005's dependency revision vector and ADR-0002's prohibition therefore does not need resolving. It cannot arise: a managed revision has no external dependencies to record.

**External to managed** has a genuine argument for being permitted: Types Registry controls the managed target — it governs its evolution, evaluates its availability locally, and can refuse to delete it. That is all true, and it is not sufficient.

* **Deletion safety rests on data the platform cannot verify.** Registering edges removes the dependence on plugin *uptime*, which was the stated goal, and replaces it with a dependence on plugin *diligence*. A plugin that never registers an edge produces a registry that believes a managed type has no dependents, and Types Registry has no way to notice: the absence of a registration is indistinguishable from the absence of a dependency. The failure is silent, it breaks a consumer, and nothing repairs it. It is tempting to answer that the failure direction is safe because a stale edge over-blocks, but that reasoning covers an edge that exists and is wrong, not an edge that was never sent.
* **The ordering invariant is unenforceable.** "Registration must be committed before the plugin serves the dependent entity" is a rule about the plugin's internal sequencing. Types Registry cannot observe it and cannot test for it outside a conformance suite the plugin runs on itself.
* **It buys a permanent write path for one purpose.** Plugins would move from read-only to read-write, acquiring authentication, per-claim authorization, two-way idempotency, retry safety, and explicit withdrawal — and the operational failure mode that a plugin which withdraws incorrectly permanently blocks a deletion until an operator intervenes. All of it exists solely to protect a relationship that the closed boundary removes.
* **It leaves one availability relationship evaluated by a different mechanism from every other.** ADR-0010's blocking closure is a relation over managed entities. An external subject depending on a managed target has no row in managed storage, so that one relationship kind would be evaluated live, by the source, while all the others are evaluated in SQL — two mechanisms for one semantic.

### The external half of the rule is only half enforceable

The prohibition on an Externally Managed Entity depending on a Managed Entity has two cases with completely different enforcement status, and conflating them would leave a false impression of safety.

**Derivation across the boundary is structurally impossible.** A derived identifier `A~B~` has first segment `A`, and Source Claims are rooted single-segment patterns, so the owning source of `A~B~` and of `A~` is necessarily the same claim. An external derived type therefore cannot have a managed base. This is identifier arithmetic, not a rule anything has to check.

**A reference from inside an external schema document is representable and undetectable.** The claim grammar constrains identifiers, not content. An External Registry Source is outside the platform's control and its implementation may well permit a `$ref` or `x-gts-ref` to a managed identifier, and ADR-0002 forbids Types Registry from interpreting source-owned content, so the platform neither sees nor checks it. The `MUST NOT` for this case is therefore a statement about what the platform recognizes, not about what a source can publish.

**Types Registry consequently offers no guarantee for such a reference.** Admission-time validation applies to Managed Entities only; for a live external result the platform validates the response envelope and its own invariants — identifier integrity, derived reference equality, claim conformance, entity kind, revision and hash consistency — and nothing about content. Specifically, for a cross-boundary content reference:

* **Deletion safety does not extend to it.** The reference is not a registered dependent and cannot become one, since plugins have no write path. The managed target can be deleted with no block, no registry event, and no way for the source to have been consulted.
* **Availability does not propagate to it.** ADR-0010's blocking closure holds between Managed Entities only, so an unavailable managed target does not make the external entity `UNAVAILABLE` through the registry.
* **No dependent-specific revalidation runs for it.** ADR-0005 revalidates every affected *registered* dependent in the transitive closure before a new managed revision becomes current. An unregistered external referrer is not in that closure, so when the managed target advances a revision nobody asks whether the external schema is still coherent.
* **Lifecycle transitions are not reported to it.** The platform does not notify a source that a managed target was deleted, and reference validation on the external side is source-owned.
* **Purge can silently rebind it.** ADR-0013 releases the identifier, so a purged-then-re-registered identifier resolves to a different logical entity while the external reference still names it. This is the same hazard purge carries for domain rows, and it applies to external references with no mitigation at all.

One guarantee is **not** lost, and the distinction matters because overstating it in the other direction would be equally wrong. ADR-0003's backward-compatibility guarantee is unconditional on the managed entity's own revision chain and does not depend on who consumes it: `Valid(current) ⊆ Valid(candidate)` holds regardless. An external consumer whose instances validate against a managed Type Schema is therefore protected exactly as any consumer is. What it lacks is the dependent-specific check above, not the compatibility mode itself.

Making such a reference detectable was considered and is rejected permanently rather than deferred — see *Sub-choices within the selected option*, below.

### Source Claims are rooted single-segment patterns

A Source Claim pattern **MUST** consist of exactly one segment — it contains no `~` — and **MUST** carry the wildcard token at a token boundary within that segment. The admissible forms are therefore:

```text
gts.<vendor>.*
gts.<vendor>.<package>.*
gts.<vendor>.<package>.<namespace>.*
gts.<vendor>.<package>.<namespace>.<type>.*      -- any major version
```

The single-segment requirement is what makes disjointness structural rather than a per-reference check at admission. **The owning claim of any identifier is determined by its first segment alone.** A derived identifier `A~B~` has first segment `A`, so the entire derivation chain of an externally managed entity necessarily falls inside the claim that owns its root segment; combined with ADR-0007's existing rule that a claim may not overlap the managed identifier space, no chain can begin in one universe and continue in the other.

It also makes the genuinely dangerous form unrepresentable. A mid-chain claim such as `gts.cf.core.events.type.v1~acme.*` slices into a chain whose base segment is managed, and every nesting pathology grows from there.

Chain coverage itself is supplied by the matcher rather than by a separate rule. `gts-id` matches segment-wise and field-wise; a wildcard token replaces a whole token, and once a wildcard segment is reached it accepts every remaining segment including the chain separator. This is the "implicit derived-type coverage" of GTS §3.6, and under the closed boundary it is exactly what a claim needs.

Overlap is decided by the platform matcher's containment primitive, which for rooted single-segment patterns reduces to "one field list is a prefix of the other". It runs over the whole claim set, with no stored upper bound and no index to narrow candidates first. Claim counts are expected to remain in single digits because the grammar permits broad vendor, package, and namespace prefixes instead of requiring one claim per versioned type. A string range would therefore only pre-filter candidates that the matcher must confirm anyway. `database.sql` records the same beside the table.

Deciding overlap is not the whole of enforcing it. The invariant is the *absence* of an overlapping row, which no unique index expresses and no row can be locked to protect, and the check runs outside the commit transaction on the asynchronous write path — so two activations, or an activation and a managed registration, could each observe no overlap and both commit. The serialization point that closes that window, and the routing generation advance that commits with it, are specified in [DESIGN §3.2, *Registry Source Plugin registration*](../DESIGN.md#registry-source-plugin-registration).

The cost that remains is real and must be documented for vendors: **an external claim and the managed identifier space cannot nest.** `gts.acme.*` covers everything under that vendor including every chain beneath it, so a vendor integrating an external source partitions its prefixes — some served externally, some registered as managed — rather than placing the latter underneath the former.

### Deletion safety is entirely local

Deletion of a Managed Entity is decided from managed storage alone. It calls no plugin, reads no plugin-supplied data, and depends on neither plugin uptime nor plugin diligence.

Types Registry examines only dependencies it owns and can enumerate: derived types, schemas holding a `$ref` to the target, and registered Instances conforming to it. `x-gts-ref` creates no dependency and therefore does not block deletion.

**Live reverse-impact query is not retained at all,** and closing the boundary is what emptied it. It could report nothing **platform-authoritative** about a Managed Entity: no externally managed entity may depend on one, so any dependent a source named would be one the rule forbids and the platform does not recognize — and, because a reference from inside an external document is undetectable here, the platform could neither confirm nor refute it. What remained was external dependents of an *externally managed* entity: a question entirely inside the source's own universe, which the source's own tooling answers better.

An optional diagnostic also has no consumer. Types Registry exposes no dependent-enumeration operation on either plane; callers instead ask *would this mutation be refused, and why?* through that mutation's Dry Run.

A reverse-impact report would therefore have a producer and router but no caller. ADR-0007 keeps it out of the contract rather than making it optional. The contract consequently has no advisory tier, and `cpt-cf-types-registry-principle-fail-closed` needs no exception for a warning-only plugin output.

Adding both a diagnostic and a surface that renders it later remains additive.

**Deletion is independent of all of this in both directions.** It consults no source and is blocked by nothing a source could report, because it reads managed storage alone. A source that has built on a managed contract in a way the platform cannot see does not gain a veto by reporting it.

The principle this was an example of is unaffected: authoritative decisions read local state. What changed is that there is no informational query left for the second half of that sentence to permit.

### A retired Source Claim reserves its space until purge

A Registry Source Plugin is a registered Instance, so its Source Claims follow ordinary lifecycle. Deleting the Instance retires its claims and retains them as **reservations**. Types Registry then refuses both:

* a Managed Entity matching a retired claim;
* another plugin claim overlapping it.

Retirement needs no separate operation. It is governance, not a liveness observation: an unreachable plugin keeps its claims, and requests needing it fail closed. Claimed space must not flicker with plugin health.

Without the reservation, the same identifier could be re-registered as managed and deterministic Registry References would silently rebind persisted domain references to a different entity.

A reservation is permanent under ordinary operation and is released by exactly one thing: purging the plugin Instance, under ADR-0013. That is the same single named exception that releases a managed GTS Identifier, and it is the same hazard — a released space can be re-registered, deterministic derivation reproduces the reference, and a stored domain row rebinds. Treating the two differently would have been an asymmetry with no argument behind it, so purge is disabled by default here for the reason it is disabled there.

This costs a table of patterns. It persists no external entity identifiers — Types Registry already persists Source Claims, and a retired claim is the same kind of data — so it does not breach the persistence rule.

Reservation alone is not sufficient, and the gap is stated rather than hidden. It prevents an identifier from rebinding to a Managed Entity, but a **successor plugin** serving the same claim could serve different content under the same identifier, and deterministic Registry References would rebind persisted domain data to it.

**There is therefore no claim takeover operation.** Activating a plugin claim that overlaps a retired reservation is rejected with no exception and no declared-intent escape. A governed takeover was considered and is not built, because the assertion it would carry cannot be checked by anything: this ADR's own persistence rule leaves Types Registry holding no identifier, revision, or content hash of what the predecessor served, so a successor's claim of continuity would be compared against nothing. An unverifiable assertion accepted through an API reads as a check and is a formality, which is the shape `cpt-cf-types-registry-principle-local-authority` exists to refuse.

Two paths remain for reusing a reserved space, and neither is an ordinary operation:

* **Purge the plugin Instance** (ADR-0013), which removes the reservation and releases the space to whoever asks next, including a managed registration. Disabled by default.
* **A database migration shipped with Types Registry**, which retargets the claim rows to a named successor. This is the narrower of the two — the space is never unreserved and never becomes registrable by an unrelated party — and its cost of entry is an operator with database access and a reviewed migration, which is the ceremony proportional to an act that silently rebinds persisted domain references.

The migration must follow the routing-writer protocol and keep the successor's Instance document consistent with the claim projection; otherwise pods may retain stale routing or a later plugin revision may undo the retargeting. [DESIGN §3.2, *Registry Source Plugin registration*](../DESIGN.md#registry-source-plugin-registration) defines the transaction order.

Ordinary plugin replacement does not need either. A Registry Source Plugin is a registered Instance and its content is mutable under ADR-0006, so upgrading the implementation behind a claim is a new content revision of the same Instance and touches no reservation. Only a change of the plugin's own GTS Identity reaches this rule, and that is rare enough to be worth an operator.

### What a vendor does instead

A vendor that wants a type derived from a platform contract registers it as a **Managed Entity**, through the ordinary write path, where every platform guarantee applies to it: one enforced compatibility mode, local availability evaluation, dependency-safe deletion, and a stable Registry Reference. This is the normal path for the XaaS Vendor Architect, not a workaround.

The managed profile it lands in is narrower than the GTS grammar, and this actor should know which narrowings it will meet. One it might have feared is not there: a vendor arriving with a minor-versioned catalogue does not have to **flatten** it into major-only identifiers, because ADR-0004 admits minor versions under every prefix — the platform keeps its own contracts major-only through a lint over their source, not through an admission rule.

It may have to **renumber** a catalogue whose minors have gaps or do not start at `.0`. ADR-0004 requires contiguous minors from `M.0`, making the compatibility baseline a function of the candidate identifier rather than registry state.

For example, upstream `v1.0`, `v1.3`, `v1.7` can become managed `v1.0`, `v1.1`, `v1.2`, but the identifiers then diverge from upstream numbering. The actor has three choices:

* accept that renumbering;
* publish a new major;
* keep the self-contained catalogue in an External Registry Source, where platform numbering rules do not apply.

An External Registry Source is for a vendor whose type universe is **self-contained** — a pre-existing registry that is authoritative for its own contracts and does not build on the platform's. That is the case federation was introduced to serve.

The composition story therefore sits on the managed side of the boundary rather than the external one. A vendor does not express it in its own registry and register a dependency edge back; it registers the derived type here, which is the only side on which the platform can keep its promises about it.

### Consequences

* ADR-0002's enumeration of prohibited persistence holds with no exception, so the persistence rule and the enumeration state the same thing.
* The P1 federation contract of ADR-0007 contains neither dependency registration nor reverse dependency-impact lookup: the boundary leaves nothing to register, and nothing to report that any operation would consume. Everything left in the contract is mandatory, so it has no optional or advisory tier, nothing about it is declared per plugin, and plugin conformance tests need only one shape.
* **Registry Source Plugins have no write path to Types Registry.** The relationship is read-only, so the authentication, per-claim authorization, idempotency, and withdrawal semantics such a path would need do not arise.
* Every availability-blocking relationship in ADR-0010's table holds between two Managed Entities, and none crosses the boundary.
* Managed deletion, resolution, and availability involve no plugin and no plugin-supplied data. The managed lookup NFR is achievable without plugin-latency budgeting, and no degraded vendor integration can block or corrupt a managed decision.
* An external claim and managed identifiers cannot nest, so vendor namespace planning acquires a constraint it did not have.
* A vendor cannot publish, in its own external registry, a type derived from a platform type. This is a genuine loss of expressiveness and the remedy is managed registration.
* Storage simplifies: a dependency edge has two managed endpoints, both non-null, so the edge set needs no external label columns, no discriminating CHECK constraints, and no unique index over nullable columns.
* The boundary is declared in both directions but enforced in one. A source that references a managed contract from inside its own schema receives none of the platform's dependency guarantees, and the platform cannot tell that it happened. This is a documented gap in the integration contract, not an oversight, and it has to be communicated to vendors rather than only recorded here.
* Re-opening the external-to-managed direction later is additive: it would need a registration path and would reintroduce the diligence dependence recorded above. Re-opening managed-to-external still requires source-side change notification, a per-revision monotonicity assertion, and a resolution of the availability cost.

### Confirmation

This decision is confirmed when:

* admission rejects a managed Type Schema containing a `$ref` or `x-gts-ref` to an externally managed target, and rejects a managed identifier deriving from an externally managed base, with diagnostics naming the offending reference;
* Source Claim activation rejects a pattern covering an `x-gts-ref` authority identifier in current Managed Type Schema content;
* no admitted managed revision contains an external revision token or content hash in its provenance record;
* Types Registry storage contains no external entity identifier in any column, including on dependency edges;
* an externally managed entity cannot be admitted as derived from a managed base, and the impossibility follows from claim selection on the first segment rather than from a check that could be bypassed;
* a managed entity referenced from inside an external schema document is deletable, purgeable, and revisable without obstruction, and no availability verdict or revalidation reflects that reference — the documented gap is exercised as a test rather than assumed;
* no federation response validation parses returned content in order to detect a cross-boundary reference;
* a Source Claim is rejected at activation unless its pattern is exactly one segment carrying a wildcard at a token boundary; in particular a multi-segment pattern such as `gts.cf.core.events.type.v1~acme.*` is rejected;
* the owning claim of an identifier is selected from its first segment alone, and a claim covering `gts.vendor.*` also covers `gts.vendor.foo.invoice.type.v1~acme.crm.bar.type.v1~`, with registration of a managed entity at any identifier inside the claim rejected as overlapping it;
* claim-overlap detection treats two patterns as overlapping exactly when one covers the other, and for this grammar that reduces to one field list being a prefix of the other;
* Types Registry exposes no plugin-callable operation that creates, modifies, or withdraws registry state;
* deleting a Managed Entity with no managed dependent succeeds while every plugin is unreachable, and deleting one with a managed dependent is rejected while every plugin is unreachable — both proving the decision used local state only;
* registering a Managed Entity whose identifier matches a retired Source Claim is rejected, and deleting a plugin Instance retires its claims into exactly that state while an unreachable plugin retains its own;
* activating a new plugin's claim over a retired claim is rejected with no exception, and no request field, declared intent, or continuity assertion makes it succeed;
* the retargeting migration is documented as carrying the hazard it cannot dispel rather than as a safe path: the persistence rule leaves the platform holding no identifier, revision, or content hash of what the predecessor served, so nothing verifies that the named successor serves the same logical entities, and a successor that serves different content under a reserved identifier silently rebinds every domain row holding its Registry Reference. What stands behind continuity there is the review of that migration, never a registry check;
* no plugin operation asks a source about dependents, and no plugin output is permitted to degrade with a warning in place of failing closed — the contract has no optional or advisory tier to test.

## Pros and Cons of the Options

### Closed both ways

* Good, because no platform guarantee depends on a counterparty's uptime *or* diligence, and the incompleteness that a registered-edge model cannot detect becomes impossible rather than unlikely.
* Good, because plugins stay read-only: no write path, no plugin authorization surface, no idempotency and withdrawal semantics, no operator-recoverable state left behind by a misbehaving plugin.
* Good, because it dissolves the ADR-0005 / ADR-0002 provenance conflict and restores the persistence rule to a form with no exceptions.
* Good, because every availability-blocking relationship is then between two managed entities and is evaluated by one mechanism.
* Good, because disjointness of the identifier spaces is structural: with rooted single-segment claims the owning source of an identifier follows from its first segment, so a derivation crossing is unrepresentable rather than checked.
* Neutral, because a reference from inside an external schema document remains representable by the source and undetectable by the platform. This option does not fix that; it makes the resulting exposure narrow and stateable, which the asymmetric option did not, since there the same reference would have been expected to arrive as a registration and its absence would have looked like absence of a dependency.
* Bad, because a vendor cannot build a type in its own registry on a platform contract, which is a real composition scenario and the strongest argument against this option.
* Bad, because external claims and managed identifiers cannot nest, constraining vendor namespace layout.
* Bad, because a rooted claim is broader than a complete-identifier claim — it captures every chain beneath it — so a claim captures more than its author may intend and the blast radius of a mis-specified claim grows.

### Asymmetric, with registered external dependents

* Good, because it keeps the intended direction of reuse open: a vendor's own registry can build on platform contracts.
* Good, because deletion safety does not depend on plugin uptime, and a stale edge over-blocks rather than under-blocks.
* Good, because it replaces an unverifiable "no false negatives" obligation with a positive act the platform can observe.
* Bad, because the act is only observable when it happens. A missing registration is indistinguishable from an absent dependency, so the platform's confidence rests on plugin diligence and its failure is silent and unrepairable.
* Bad, because "register before serving" is a rule about plugin internals that Types Registry cannot enforce.
* Bad, because plugins acquire a permanent write path, and a plugin that withdraws incorrectly blocks a deletion until an operator intervenes.
* Bad, because it forces one availability relationship kind to be evaluated live while all others are evaluated in SQL.

### Open, with a provenance carve-out

Types Registry would store the external revision token inside the immutable managed revision record, marked as provenance and never read for serving or freshness.

* Good, because it keeps the composition story open in both directions.
* Good, because a write-once provenance token creates no second source of truth, so it does not breach the intent of ADR-0002.
* Bad, because it addresses only the provenance conflict and leaves compatibility, deletion safety, and availability broken in exactly the ways listed above.
* Bad, because a stored external revision is a standing invitation to use it as a freshness check, which would create the second source of truth the rule prohibits.

### Query every plugin for reverse impact at deletion time

Retained here because it was the alternative to registration under the asymmetric option, and its analysis is what makes the closed option preferable to either.

* Good, because the source stays the single owner of its own dependency knowledge and plugins need no write path.
* Good, because there is no registration to become stale.
* Bad, because an authoritative decision depends on the uptime of every claiming plugin: one degraded vendor integration blocks deletion of unrelated platform types.
* Bad, because correctness rests on a "no false negatives" promise verifiable only by conformance tests, and a plugin that under-reports causes silent breakage rather than a visible failure.

### Sub-choices within the selected option

Alternatives considered while shaping the option above, recorded here rather than in *Decision Outcome*, which states what was chosen.

**Making the reference detectable was considered and permanently rejected.** The router could parse returned content, extract GTS references, and reject references outside the source's claims. This is declined because:

* parsing would enter the live read path and cannot always run — reverse resolution need not return a document;
* Types Registry would interpret source-owned content to enforce a platform rule, contrary to ADR-0002, and acquire content-validity failures it does not own;
* an existing vendor registry referencing platform contracts would become impossible to integrate rather than integrable with a documented gap.

The boundary is therefore deliberately declared in both directions and enforced in one.

The alternative grammar — a claim matches a complete canonical identifier and never by chain prefix — would serve one purpose: stopping a plugin that claims a base type's namespace from silently capturing managed types derived from that base. It has to work *against* the matcher's implicit coverage to do so, and the protection is unnecessary once managed derivation from an external base is forbidden outright, because no managed derivation inside an external claim remains to capture.

**The wildcard position is deliberately not pinned to the version.** Version-only wildcarding would reduce overlap to pattern-string equality, but at substantial operating cost:

* one claim per type family — hundreds for a large source;
* a Types Registry control-plane change for every new vendor type;
* silent failure when a new type lacks a claim and becomes registrable as managed.

That is a coarse projection of source inventory, precisely the coupling ADR-0002 avoids. Marginally simpler overlap detection does not justify it.

## More Information

### Industry Practice

* [Kubernetes owner references and finalizers](https://kubernetes.io/docs/concepts/overview/working-with-objects/owners-dependents/) record dependency edges in the API server rather than discovering them by polling controllers, and a finalizer blocks deletion from stored state. Deletion safety is local; reconciliation is not on the deletion path. Note the difference in trust: a Kubernetes controller writing a finalizer is inside the cluster's trust and correctness boundary, whereas a vendor plugin registering an edge is not — which is why the same mechanism does not transfer.
* [Terraform](https://developer.hashicorp.com/terraform/language/state) keeps the dependency graph in its own state rather than re-deriving it from providers at destroy time, for the same reason: an authoritative destroy ordering cannot depend on provider availability.
* [Confluent Schema Registry](https://docs.confluent.io/platform/current/schema-registry/fundamentals/schema-evolution.html) has no cross-registry reference model at all. A subject's references resolve inside one registry, and federation across registries is an import or replication concern rather than a reference concern — the same separation this decision adopts.

## Traceability

- **PRD**: [../PRD.md](../PRD.md)
- **DESIGN**: [../DESIGN.md](../DESIGN.md)
- **ADR-0001**: [0001-cpt-cf-types-registry-adr-storage-identity-query-model.md](./0001-cpt-cf-types-registry-adr-storage-identity-query-model.md)
- **ADR-0002**: [0002-cpt-cf-types-registry-adr-external-source-live-delegation.md](./0002-cpt-cf-types-registry-adr-external-source-live-delegation.md)
- **ADR-0003**: [0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md](./0003-cpt-cf-types-registry-adr-type-schema-evolution-compatibility.md)
- **ADR-0005**: [0005-cpt-cf-types-registry-adr-type-schema-revisions.md](./0005-cpt-cf-types-registry-adr-type-schema-revisions.md)
- **ADR-0007**: [0007-cpt-cf-types-registry-adr-federated-source-routing-query.md](./0007-cpt-cf-types-registry-adr-federated-source-routing-query.md)
- **ADR-0010**: [0010-cpt-cf-types-registry-adr-tenant-availability-evaluation.md](./0010-cpt-cf-types-registry-adr-tenant-availability-evaluation.md)
- **ADR-0013**: [0013-cpt-cf-types-registry-adr-platform-purge-of-deleted-entities.md](./0013-cpt-cf-types-registry-adr-platform-purge-of-deleted-entities.md) — decides the purge that releases a retired Source Claim reservation, the single exception to the permanence this ADR gives it.
- **Database reference schema**: [../database.sql](../database.sql)

This decision directly addresses:

* `cpt-cf-types-registry-fr-externally-managed-entities` - restores the enumerated persistence prohibition and closes the boundary in both directions.
* `cpt-cf-types-registry-fr-ref-tracking` - makes the tracked dependency set entirely managed, so deletion safety needs no plugin-supplied data.
* `cpt-cf-types-registry-fr-registry-source-routing` - defines the rooted single-segment claim grammar and makes a retired claim a reservation, released only by the purge of ADR-0013.
* `cpt-cf-types-registry-fr-validate-type-derivation` - forbids derivation across the boundary in both directions.
* `cpt-cf-types-registry-fr-id-resolution` - closes the rebinding path opened by plugin retirement, and leaves reuse of a reserved space to purge or to an operator-applied migration rather than to a runtime operation.
* `cpt-cf-types-registry-nfr-lookup-latency` - keeps plugin calls out of the managed resolution and availability paths.
