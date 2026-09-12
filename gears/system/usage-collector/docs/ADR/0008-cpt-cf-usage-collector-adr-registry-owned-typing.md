---
status: accepted
date: 2026-08-17
decision-makers: usage-collector spec owners
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

# Type declarations owned by types-registry, resolved and cached by the gear

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Registry-owned declarations with a gear-side resolver cache](#registry-owned-declarations-with-a-gear-side-resolver-cache)
  - [Plugin-owned catalog co-resident with usage entries](#plugin-owned-catalog-co-resident-with-usage-entries)
  - [Gear-local catalog table](#gear-local-catalog-table)
  - [No declarations at all](#no-declarations-at-all)
- [More Information](#more-information)
  - [Open question — namespace ownership across gears](#open-question--namespace-ownership-across-gears)
  - [Related decisions](#related-decisions)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-registry-owned-typing`

## Context and Problem Statement

Every entry the gear accepts names a GTS type, and that reference is all the
entry says about its own type. Four attributes sit behind the reference, in the
declaration: the aggregation fold, the canonical metering unit, the metadata
surface, and the retention policy.

The gear therefore cannot interpret a persisted entry without the declaration. It
cannot fold a series, name the unit of a quantity, or validate metadata. Who owns
a declaration is a first-order contract question, not a storage detail.

Two gears resolve the same meters. Quota Enforcement resolves them against the
same platform type system, and the platform runs `types-registry`, whose job is
to hold type declarations. A declaration private to one gear cannot serve a
second consumer.

Resolution also sits on the ingestion hot path, and `types-registry` publishes
neither a latency obligation nor an availability obligation of its own. This
decision answers two questions. Which component owns a type declaration? How does
this gear reach one without making its own ingestion obligations contingent on a
second gear?

## Decision Drivers

- One source of truth at platform scope — two gears resolve the same meters. The
  single-declaration property must therefore hold across gears, not inside one.
- No gear's private schema as another gear's contract — a storage plugin's tables
  are an implementation detail, and no other gear reads them.
- Self-contained ingestion obligations —
  `cpt-cf-usage-collector-nfr-ingestion-latency` allows no registry round trip
  per entry in the steady state.
- Fail-closed typing — the attributes that give an entry its meaning exist only
  in the declaration, and a substituted default reprices history silently.
- Smallest surface — the gear must carry no operation whose system of record
  is another gear.
- Immutability as the warrant for the cache — a declaration that cannot change
  meaning is safe to cache with no bound on validity.

## Considered Options

- Registry-owned declarations with a gear-side resolver cache —
  `types-registry` owns every declaration and its whole lifecycle, and the Type
  Resolver caches what it resolves.
- Plugin-owned catalog co-resident with usage entries — the storage plugin holds
  a catalog table beside the entries that reference it. The gear mutates that
  table through its own surfaces.
- Gear-local catalog table — the gear holds its own catalog table, populated from
  operator configuration at startup. It reaches no other component to resolve a
  type.
- No declarations at all — an entry carries any type reference, and each consumer
  decides what it means.

## Decision Outcome

Chosen option: "Registry-owned declarations with a gear-side resolver cache". It
is the only option that keeps one declaration per meter platform-wide. Both gears
that resolve a meter read the same component.

The three alternatives each fail on that point. A plugin-owned catalog gives the
single-source property at gear scope only, and reaching it from a second gear
means exposing this gear's plugin-private schema. A gear-local table is a second
source of truth for a platform-wide declaration. Free-form references leave the
fold, the unit, and the metadata schema with nowhere to live.

The decision pins seven statements.

1. **The gear owns no type catalog.** `types-registry` holds every declaration
   and owns its whole lifecycle. The gear persists no catalog, and denormalizes
   no declared attribute onto an entry.
   `cpt-cf-usage-collector-adr-declaration-rehydration` amends this statement
   with one temporary exception: a recovery mirror the gear reads only to put a
   forgotten declaration back. It is deleted when `types-registry` gets
   persistent storage.
2. **The Type Resolver resolves on the cold path and caches the result.** On a
   cache miss it consults `types-registry`. In the steady state it serves
   resolution from the cache, so ingestion performs no registry round trip per
   entry.
3. **Declaration immutability makes indefinite cache validity safe.** A
   declaration is immutable in its fold, its unit, and its metadata surface. A
   cached declaration therefore stays usable while `types-registry` is
   unreachable. A registry outage degrades the introduction of new meters rather
   than the ingestion of existing ones. Retention is not on that immutable list,
   and it is not cached either: the resolver never serves it, and the storage
   plugin reads it from `types-registry` on its own. An amended retention
   therefore reaches the only component that applies it, and cache validity does
   not depend on it.
4. **Resolution failure is fail-closed.** Where a type reference does not
   resolve, the gear rejects the operation with an actionable error that names
   the identifier. It substitutes no default fold, unit, or metadata surface, and
   relaxes no validation to protect availability. This holds on the write paths
   and the read paths alike.
5. **No type operation of any kind exists on any surface — read or write.**
   REST, the SDK, and the Plugin SPI carry no type operation. The Plugin SPI
   carries no catalog method either. A storage plugin never persists a
   declaration, serves one, or enforces referential integrity against one. An
   operator reads what this deployment accepts from `types-registry` directly.
   The restore path of `cpt-cf-usage-collector-adr-declaration-rehydration` is
   internal repair and adds no operation to any of the three surfaces.
6. **Plugin binding is a separate use of the same registry.** `types-registry`
   serves this gear twice over. It resolves type *declarations*, which this
   decision covers. It also resolves the configured GTS selector to the bound
   storage-plugin *instance*, which `cpt-cf-usage-collector-contract-gts-registry`
   and `cpt-cf-usage-collector-adr-pluggable-storage` cover. The two uses have
   different failure modes and different mitigations, and no statement here
   applies to plugin binding.
7. **Every meter is a derived type of a reserved base.** A declaration this
   gear meters against is a GTS *type*, terminated by `~`, deriving from
   `gts.cf.core.uc.usage_record.v1~` with exactly one further segment. The base
   is abstract, defines the form of an entry, and carries the
   `x-gts-traits-schema` that makes the fold, the canonical unit, and the
   retention policy required of any concrete meter. Derivation is what makes a
   declaration checkable at all: `types-registry` validates those attributes
   when the declaration is registered, rather than this gear discovering an
   unmeterable declaration when it rejects the first entry against it. A
   declaration sitting under any other base is not a meter this gear serves.

### Consequences

- A second gear sits on the ingestion path. The resolver cache confines that
  exposure to the introduction of new meters, and the cold-cache and
  registry-unreachable load tests verify the confinement.
- Referential integrity between an entry and its declaration is not a database
  constraint. Removal of a still-referenced declaration therefore fails every
  operation that must resolve it. The affected entries stay persisted and
  unmodified, and the gear cannot interpret them. Whether a declaration is ever
  removed is a `types-registry` decision. Today the registry publishes no
  removal operation on any of its layers, so this case is not reachable, and a
  missing declaration means the registry lost it. Adding removal is subject to
  the condition in `cpt-cf-usage-collector-adr-declaration-rehydration`.
- The Type Resolver carries two observability signals, resolution failure and
  cache staleness, both under
  `cpt-cf-usage-collector-nfr-operational-visibility`.
- Type declarations sit outside the gear's consistency floor. Their propagation
  is a property of the resolver cache rather than of a plugin read path.
  `cpt-cf-usage-collector-adr-consistency-contract` states that carve-out, and
  `cpt-cf-usage-collector-nfr-query-freshness` inherits it.
- `types-registry` must support a declaration lifecycle that its own published
  requirements do not yet cover. This gear does not deliver that lifecycle, and
  the PRD records the dependency as an assumption.
- The reserved base buys registration-time enforcement at the cost of
  namespace freedom. A gear that already declares meters under a base of its
  own must re-base them to be metered here, and the closed unit list means a
  declaration naming a unit outside it cannot be registered at all rather than
  being registered and left unusable.

### Confirmation

- **Cold-cache load test.** Ingestion runs at the throughput-profile envelope
  with the declaration cache empty, and stays inside
  `cpt-cf-usage-collector-nfr-ingestion-latency`.
- **Registry-unreachable load test.** Ingestion of already-resolved meters runs
  with `types-registry` unreachable, and stays inside the same budget. A cached
  declaration therefore survives the outage.
- **Fail-closed test.** An unresolvable type reference causes rejection on the
  write path and the read path. The test asserts rejection rather than a
  defaulted fold or unit.
- **Surface test.** No REST endpoint, no SDK method, and no Plugin SPI method
  offers a type operation of any kind, read or write.

## Pros and Cons of the Options

### Registry-owned declarations with a gear-side resolver cache

`types-registry` owns every declaration, the Type Resolver caches what it
resolves, and no surface of this gear exposes a type operation.

- Good, because one component owns the declaration for the whole platform, so
  every gear that resolves a meter reads the same declaration.
- Good, because this gear takes no dependency on another gear's private schema,
  and exposes none of its own.
- Good, because immutability makes the cache safe with no staleness bound. Only
  additions and withdrawals have to propagate.
- Good, because the gear's write surface shrinks. Registration and deletion
  belong to the component that owns the lifecycle.
- Good, because a new meter becomes usable with no Usage Collector code change,
  configuration change, or redeployment.
- Neutral, because a second gear sits on the ingestion path. The cache confines
  the exposure to the introduction of new meters.
- Bad, because the declaration lifecycle becomes another gear's deliverable, and
  removal of a declaration sits outside this gear's control.
- Bad, because the database enforces no referential integrity. The gear enforces
  it fail-closed at resolution time, which detects an orphan reference rather
  than prevents one.

### Plugin-owned catalog co-resident with usage entries

The storage plugin holds a catalog table beside the usage entries. The gear
mutates it through its own SDK and REST surfaces, under PDP authorization.

- Good, because one store holds the catalog rows and the entries that reference
  them. An operator and a consumer point at one place.
- Good, because referential integrity is enforceable natively. A foreign key
  from the entry to the catalog row makes an orphan reference impossible.
- Good, because resolution needs no second gear and no cache. The ingestion path
  reaches one storage dependency and stops there.
- Neutral, because catalog mutation runs through the same PDP-authorized path as
  every other write the gear serves.
- Bad, because a second gear resolves the same meters. A declaration inside this
  gear's storage plugin makes one gear's plugin-private schema another gear's
  dependency.
- Bad, because the platform then holds two catalogs of the same meters. The
  single-source property holds inside this gear and fails across the platform,
  which is the same defect one scope up.
- Bad, because every storage-plugin author must implement catalog persistence,
  with its referential-integrity and deletion rules, in each backend dialect.

### Gear-local catalog table

The gear holds its own catalog table, populated from operator configuration at
startup, and reaches no other component to resolve a type.

- Good, because the Plugin SPI carries no catalog surface. A plugin author
  implements the entry path only.
- Good, because resolution is local. No round trip to another component sits on
  the ingestion path, and no cache is needed.
- Neutral, because operator configuration is an audited change path in most
  deployments.
- Bad, because it is a second source of truth for a platform-wide declaration.
- Bad, because a declaration held inside the gear is invisible to Quota
  Enforcement, which resolves the same meters. Each gear carries its own copy of
  one fact.
- Bad, because a configuration-loaded catalog has no durable lifecycle.
  Correcting an operator mistake means editing files and restarting a replica.

### No declarations at all

No component declares a fold, a unit, or a metadata surface, and each consumer
decides for itself what a meter means.

- Good, because the gear takes no typing dependency at all. Ingestion validates
  the entry shape and nothing more.
- Good, because a new meter needs no registration step in any component.
- Bad, because the fold, the unit, and the metadata schema then have nowhere to
  live. Two consumers can fold one meter differently, and neither is wrong.
- Bad, because a quantity carries no unit of its own. A consumer that reads the
  wrong unit misprices, and nothing in the platform detects the error.
- Bad, because metadata validation disappears. A typo in a metadata key becomes a
  new grouping dimension rather than a rejection.
- Bad, because `cpt-cf-usage-collector-fr-aggregation-fold` and
  `cpt-cf-usage-collector-fr-metering-unit-binding` then have no declaration to
  read, so neither requirement can be met.

## More Information

### Open question — namespace ownership across gears

Quota Enforcement resolves under its own provisional GTS base,
`gts.cf.qe.metric.type.v1~`. Statement 7 settles this gear's half of the
question: a meter derives from `gts.cf.core.uc.usage_record.v1~`, so the two
are separate families rather than one. A metric declared only under the Quota
Enforcement base therefore cannot be metered here without a declaration of its
own under the reserved base.

What stays open is whether the platform aligns the families later, and in which
direction — a mapping between them, or a re-basing of one onto the other. That
is a platform-level decision. Neither the PRD nor this decision settles it, and
the PRD carries the open question.

### Related decisions

- `cpt-cf-usage-collector-adr-pluggable-storage` — decides the plugin-binding use
  of `types-registry` that this decision leaves untouched.
- `cpt-cf-usage-collector-adr-declared-fold` — fixes the fold that a declaration
  carries, which is the attribute this resolution path delivers.
- `cpt-cf-usage-collector-adr-consistency-contract` — publishes the consistency
  floor that this decision carves type declarations out of.
- `cpt-cf-usage-collector-adr-declaration-rehydration` — amends statement 1
  while `types-registry` stores declarations in memory only.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-usage-type-declaration` — what a declaration
  carries, and that it is immutable in the attributes that give a persisted entry
  its meaning.
- `cpt-cf-usage-collector-fr-usage-type-resolution` — resolution, caching, and
  fail-closed behaviour on both the write and the read paths.
- `cpt-cf-usage-collector-fr-aggregation-fold` — the fold is a declared attribute
  reached through this path, never inferred and never a request parameter.
- `cpt-cf-usage-collector-fr-metering-unit-binding` — the unit likewise, bound
  once per type in the declaration and never carried per entry.
- `cpt-cf-usage-collector-principle-registry-owned-typing` — the design principle
  that this decision codifies.
- `cpt-cf-usage-collector-constraint-no-type-catalog` — the constraint this
  decision realizes, across the wire contract, the SPI, and the entity model.
- `cpt-cf-usage-collector-component-type-resolver` — the component that owns
  resolution and the cache. It serves the write and read paths internally and
  exposes no surface of its own.
- `cpt-cf-usage-collector-contract-types-registry` and
  `cpt-cf-usage-collector-actor-types-registry` — the dependency this decision
  takes for declaration resolution.
- `cpt-cf-usage-collector-contract-gts-registry` — plugin binding resolution,
  which this decision does not change.
- `cpt-cf-usage-collector-nfr-ingestion-latency` — no registry round trip in the
  steady state, because the cache serves resolution.
- `cpt-cf-usage-collector-usecase-declare-usage-type` and
  `cpt-cf-usage-collector-usecase-declare-meter` — the use cases that run against
  `types-registry` rather than this gear.
