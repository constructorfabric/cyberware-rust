---
status: accepted
date: 2026-09-04
decision-makers: usage-collector spec owners
---

Created:  2026-09-09 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

# Forgotten declarations are restored by the gateway from a mirror table

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Gateway-owned mirror with lazy restore](#gateway-owned-mirror-with-lazy-restore)
  - [Plugin-owned catalog with plugin-side restore](#plugin-owned-catalog-with-plugin-side-restore)
  - [No mirror, reject until the registry is durable](#no-mirror-reject-until-the-registry-is-durable)
  - [Persistent storage in types-registry first](#persistent-storage-in-types-registry-first)
- [More Information](#more-information)
  - [Related decisions](#related-decisions)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-declaration-rehydration`

## Context and Problem Statement

`cpt-cf-usage-collector-adr-registry-owned-typing` puts every declaration in
`types-registry` and gives this gear no catalog of its own. That decision
assumes the registry keeps what it is given.

It does not. `types-registry` stores declarations in memory. Every declaration
registered at run time is lost when the binary restarts. Declarations that a
gear or the registry itself registers from configuration return at startup.
Declarations registered through REST do not. A restart therefore leaves the gear
rejecting entries against meters that were valid minutes earlier.

The same reset exposes a contradiction inside this gear's own specification,
unrelated to storage. A declaration carries a retention policy, and
`cpt-cf-usage-collector-fr-billing-retention-floor` obliges the storage plugin
to honor it. `DESIGN.md` §3.3 forbids a plugin from resolving a declaration. The
plugin is therefore obliged to apply a value it is not allowed to read.

This decision answers both. The lost declarations need a bridge, and persistent
storage in `types-registry` will remove it. The retention contradiction needs no
bridge and no mechanism at all: a plugin already holds a `TypesRegistryClient`,
so narrowing the prohibition is the whole of the fix. That narrowing is a
standing arrangement rather than a temporary one, and it does not retire with
the bridge.

## Decision Drivers

- Fail-closed ordering — `cpt-cf-usage-collector-fr-usage-type-resolution`
  rejects an unresolvable type before dispatch, so a forgotten type never
  reaches the plugin.
- Ingestion obligations stay self-contained —
  `cpt-cf-usage-collector-nfr-ingestion-latency` allows no extra work on the
  warm path.
- Smallest surface to delete later — the restore is a bridge, so its answer
  must be removable from one place.
- No new burden on plugin authors — the SPI is a published contract, and a
  method added for temporary work cannot be withdrawn cheaply.
- Availability of the bridge must not bound availability of ingestion.

## Considered Options

These options concern the lost declarations. The retention contradiction admits
no options: the prohibition is this gear's own, and narrowing it is the fix.

- Gateway-owned mirror with lazy restore — the gear stores each declaration it
  resolves and puts a forgotten one back on the next miss.
- Plugin-owned catalog with plugin-side restore — the storage plugin holds the
  copy, consults the registry, and restores what it finds.
- No mirror, reject until the registry is durable — accept the loss of
  run-time declarations and wait.
- Persistent storage in `types-registry` first — fix the registry rather than
  work around it.

## Decision Outcome

Chosen option: "Gateway-owned mirror with lazy restore". It is the only option
that survives the fail-closed ordering rule while adding nothing to the Plugin
SPI.

The plugin-side option is not merely less tidy. It cannot work. The gateway
rejects an unresolvable type before it dispatches, so a record naming a
forgotten meter never reaches a plugin, and a plugin-side restore never runs.
Waiting was rejected because a restart silently drops accepted usage. Fixing
the registry is the correct end state and is planned, but it is another gear's
deliverable and leaves the retention contradiction standing.

The decision pins seven statements.

1. **The gateway restores.** The Type Resolver owns the logic. No other
   component reaches `types-registry` to repair it.
2. **The gear owns one mirror table.** It holds one row per resolved type,
   carrying the type identifier and the declaration document as registered.
   It is a recovery mirror, not a catalog of record: nothing reads it except
   the restore path, no entry references it, and it enforces no referential
   integrity.
3. **Restore is unconditional.** `types-registry` does publish declaration
   withdrawal, but not on the surface this gear resolves through. The SDK trait
   carries no removal operation and no lifecycle status. Withdrawal reaches the
   resolution path only when the database-backed surface is promoted onto it —
   the same event that retires this mirror (statement 7). The two never
   coexist, so the gear applies no test and no heuristic.
4. **A failed mirror write does not reject the entry.** It costs only the
   ability to restore that type later, which is a state
   `cpt-cf-usage-collector-adr-registry-owned-typing` already accepts. The
   bridge must not bound ingestion availability.
5. **The warm path is untouched.** A resolved declaration is served from the
   cache with no registry call and no table read.
6. **The plugin reads the declared retention from `types-registry` itself.** No
   SPI method carries it, and no declared attribute is denormalized onto an
   entry. The gateway validates before it dispatches, so a type that reaches
   the plugin is a declared one. The read is not on the ingestion path: the
   plugin applies retention in its own reclamation work. A registry outage
   outliving the plugin's client cache therefore defers a sweep rather than
   refusing an entry, and the outage tolerance the resolver cache gives
   ingestion neither extends here nor needs to. This part of the decision is
   permanent.
7. **The mirror and the restore are temporary.** They are deleted when
   persistent storage reaches this gear's resolution path. Statement 6
   outlives them.

### Consequences

- The gear gains its first durable table and its first database dependency.
  `cpt-cf-usage-collector-topology-gear-runtime` no longer holds that all
  durable state sits behind the Plugin SPI.
- A meter that is declared but never resolved is never mirrored, so a restart
  loses it. Only meters that carried real traffic survive.
- A meter whose mirror write failed is lost on a restart too. Statement 4
  accepts the entry, so the declaration then exists in neither place.
- The restore needs a definite not-found answer. Where `types-registry` answers
  with an error instead, the resolver cannot tell a forgotten declaration from
  an unavailable one, and a cold cache fails closed although the row exists. In
  the v1 topology the registry is an in-process ClientHub dependency, so the
  gear and the registry restart together and this state is narrow.
- `cpt-cf-usage-collector-fr-usage-type-resolution` calls recovery
  best-effort, so neither case is a defect against it. Both are recorded here
  because this is where the mechanism lives, and the requirement deliberately
  states neither.
- The Plugin SPI is unchanged. `DESIGN.md` §3.3 narrows one statement:
  a plugin still never interprets a declaration, except the retention it reads
  from the registry on its own.
- A plugin now takes a `types-registry` dependency for retention. Every plugin
  already holds a `TypesRegistryClient` to publish its own instance, so the
  dependency is not new.
- **The mirror must not outlive the promotion.** Statement 3 is safe only while
  withdrawal is unreachable on the resolution path. Enabling withdrawal there,
  or promoting the database-backed surface while the restore is live,
  resurrects a deliberately withdrawn meter. The `types-registry` rule is that
  a withdrawn identifier is never restored or reused. This is a blocking
  condition on both changes, recorded here, not in the registry's documents.
- Where the gear and `types-registry` run in one binary they restart together,
  so the cache is empty whenever the registry is empty. Splitting them into
  separate processes voids that and requires a background reconcile.

### Confirmation

- **Registry-reset test.** Resolve a type, clear the registry, evict the
  resolver cache, and resolve it again. The second resolution restores the
  declaration and serves the entry.
- **Unknown-type test.** A type that is registered nowhere and mirrored nowhere
  is still rejected fail-closed.
- **Mirror-failure test.** Ingestion succeeds while the mirror table is
  unwritable, and the failure is counted.
- **Lost-mirror test.** A declaration whose mirror write failed is not restored
  after a registry reset and a cache eviction. Every later entry against it is
  rejected fail-closed.
- **Registry-error test.** With a cold cache, and the registry answering errors
  rather than a definite not-found answer, resolution fails closed. The mirror
  row is not served.
- **Warm-path test.** A cache hit performs no registry call and no table read.
- **SPI surface test.** No Plugin SPI method carries a declaration, a retention
  policy, or a catalog operation.

## Pros and Cons of the Options

### Gateway-owned mirror with lazy restore

The Type Resolver stores each declaration it resolves in a gear-owned table,
and registers a forgotten one back on the next cache miss.

- Good, because it sits where the ordering rule requires it. The gateway is the
  first component to need a declaration and the one that rejects without it.
- Good, because the Plugin SPI is untouched, so no plugin author carries
  temporary work and nothing has to be withdrawn from a published contract.
- Good, because retirement is one place: drop the table and the restore branch.
- Good, because the restore repairs the registry for every reader of it, not
  only for this gear.
- Neutral, because the restore runs on the cold path, which already tolerates a
  registry round trip.
- Bad, because the gear gains a durable table and a database dependency it did
  not have.
- Bad, because a meter that never carried traffic is never mirrored.

### Plugin-owned catalog with plugin-side restore

The storage plugin holds the copy beside the entries, consults the registry on
a miss, and restores what it finds.

- Good, because durable state stays behind the Plugin SPI, as
  `cpt-cf-usage-collector-adr-pluggable-storage` intends.
- Bad, because it cannot work. The gateway rejects an unresolvable type before
  dispatch, so a forgotten type never reaches the plugin.
- Bad, because making it work means dispatching before validation, which
  `cpt-cf-usage-collector-fr-usage-type-resolution` forbids.
- Bad, because every plugin author would implement the same repair logic, in
  each backend, for work that is meant to be deleted.

### No mirror, reject until the registry is durable

Accept that a restart drops run-time declarations, and reject entries against
them until the registry rewrite lands.

- Good, because nothing is built and nothing is deleted later.
- Good, because the gear keeps its stateless runtime.
- Bad, because accepted usage is silently lost. An emitter that does not retry
  loses billable consumption, and the loss surfaces on an invoice.
- Bad, because it leaves the retention contradiction standing, which no
  registry rewrite settles.

### Persistent storage in types-registry first

Give `types-registry` durable storage, and let this gear keep the model
`cpt-cf-usage-collector-adr-registry-owned-typing` already describes.

- Good, because it is the correct end state and removes this decision entirely.
- Good, because it repairs every consumer of the registry at once.
- Neutral, because the work is planned.
- Bad, because it is another gear's deliverable on another schedule.
- Bad, because it does not settle the retention contradiction, which is
  independent of registry durability.

## More Information

### Related decisions

- `cpt-cf-usage-collector-adr-registry-owned-typing` — the decision this one
  amends. Its statement that the gear persists no catalog now carries the
  mirror exception. Its withdrawal reasoning describes a registry capability
  that does not exist.
- `cpt-cf-usage-collector-adr-pluggable-storage` — the plugin binding, and the
  reason the gear names no backend of its own. The mirror table is the first
  exception and is temporary.
- `cpt-cf-usage-collector-adr-contract-stability` — the reason a temporary need
  does not earn a Plugin SPI method.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-usage-type-resolution` — the requirement carrying
  the product-level half of this decision: a registry outage or restart must
  not stop ingestion for a GTS type already in use, and recovery of a lost
  declaration is best-effort. The PRD states that need and nothing about how it
  is met. The mirror, the restore, the two cases where recovery does not apply,
  and the retirement of all three are design mechanism and live here.
- `cpt-cf-usage-collector-fr-usage-type-declaration` — the prohibition on a
  second catalog "that has to be kept in step with the registry". The mirror
  does not breach it. Declarations are immutable, so a mirrored row never needs
  reconciling against anything. That requirement is unchanged.
- `cpt-cf-usage-collector-fr-billing-retention-floor` — the per-type retention
  the plugin now reads for itself.
- `cpt-cf-usage-collector-component-type-resolver` — the component that owns
  the mirror and the restore.
- `cpt-cf-usage-collector-contract-types-registry` — the dependency this
  decision repairs rather than replaces. The contract records the one
  declaration write the restore makes.
- `cpt-cf-usage-collector-topology-gear-runtime` — the runtime that gains one
  durable table.
- `cpt-cf-usage-collector-nfr-ingestion-latency` — the budget the warm path
  keeps.
