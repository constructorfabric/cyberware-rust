---
status: accepted
date: 2026-06-02
decision-makers: Usage Collector gear owners
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-08-11 by Virtuozzo International GmbH

# Unified plugin-DB usage-type catalog and gts_id reference model

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Amendment 2026-06-02 — kind from prefix and closed metadata_fields](#amendment-2026-06-02--kind-from-prefix-and-closed-metadata_fields)
  - [Amendment 2026-06-05 — unified UsageType and dropped created_at](#amendment-2026-06-05--unified-usagetype-and-dropped-created_at)
  - [Amendment 2026-06-08 — kind moves from gts_id prefix to UsageKind enum](#amendment-2026-06-08--kind-moves-from-gts_id-prefix-to-usagekind-enum)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Keep both catalogs plus all current attributes](#keep-both-catalogs-plus-all-current-attributes)
  - [Plugin-DB catalog only with gts_id referencing and dropped attributes](#plugin-db-catalog-only-with-gts_id-referencing-and-dropped-attributes)
  - [Keep the local-from-config catalog only](#keep-the-local-from-config-catalog-only)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-0012-unified-plugin-catalog-and-gts-id-reference`

## Context and Problem Statement

Four orthogonal complications have accumulated in the Usage Collector (UC)
UsageType specification surface as ADRs 0007, 0009, and 0010 layered onto one
another: (1) a gateway-local UsageType catalog loaded from configuration coexists
with the plugin-DB catalog managed via Software Development Kit (SDK) / Representational State Transfer (REST), giving two
sources of truth for what UsageTypes exist; (2) usage records reference UsageTypes
via a Universally Unique Identifier (UUID) derived deterministically from the UsageType type identifier (UUID v5 over the
type id per Generic Type System (GTS) guide §5.1), so the reference shape is a derivation rather
than a stored identity; (3) the UsageType specification carries `parent_type_uuid`
to model an inheritance chain across catalog rows; (4) the specification
carries `x-uc-indexable` and `abstract` (`x-gts-abstract`) complexity
attributes that mark which metadata properties are queryable and which types
may carry usage rows. Each of these was justified in isolation, but together
they enlarge the specification and SDK surface beyond what the v1 quota-reporting
consumer narrowed by commit `783abdda` actually requires. The question is
whether to keep the accumulated complexity, simplify in lockstep, or partially
revert.

## Decision Drivers

- Minimize duplicate sources of truth for the usage-type catalog — operators and
  downstream consumers must be able to point at one place when asking "what
  UsageTypes exist?"
- Align UsageType identity with GTS schema identity so the wire reference does
  not depend on a derivation step that must be re-implemented identically on
  every consumer.
- Reduce specification surface to what v1 quota-reporting consumers
  (per commit `783abdda`) actually need; complexity attributes that exist only
  to enable deferred capabilities should not ship in v1.
- Preserve forward compatibility for catalog evolution — future ADRs may
  re-introduce indexability hints, inheritance, or abstract markers if and when
  a concrete consumer requires them, but v1 must not pay for them speculatively.
- Honor the platform GTS pattern: UsageTypes are GTS Type Schemas (per
  ADR-0010), and their stored `gts_id` is the natural reference key.

## Considered Options

- **Keep both catalogs plus all current attributes** — preserve the
  gateway-local-from-config catalog (ADR-0007 / 0009) alongside the plugin-DB
  catalog, keep usage records referencing UsageTypes via the uuid5-derived UUID,
  and retain `parent_type_uuid`, `x-uc-indexable`, and `abstract` on the UsageType
  specification unchanged.
- **Plugin-DB catalog only, with `gts_id` referencing and dropped attributes**
  — the plugin-DB catalog (managed via SDK/REST) becomes the sole usage-type
  catalog; usage records reference UsageTypes via `gts_id` directly (the
  uuid5-from-type derivation is removed); the UsageType specification drops
  `parent_type_uuid`, `x-uc-indexable`, and `abstract`. (chosen)
- **Keep the local-from-config catalog only** — remove the plugin-DB catalog
  surface, keep the gateway-local-from-config catalog as the sole source, and
  retain or simplify the attributes around it.

## Decision Outcome

Chosen option: **"Plugin-DB catalog only, with `gts_id` referencing and dropped
attributes"**, because it is the only option that simultaneously eliminates
the duplicate catalog source, aligns the usage-record UsageType reference with
the GTS schema identity already stored on every catalog row, and shrinks the
specification surface to match the v1 quota-reporting consumer narrowing
(commit `783abdda`). Keeping both catalogs preserves redundancy without
adding capability; keeping only the local-from-config catalog removes the
runtime SDK/REST surface that downstream operators already depend on.

The decision pins four simplifications. They are load-bearing for the cascade
phases that follow and must not be diluted by downstream artifacts:

1. **The plugin-DB catalog (managed via SDK/REST) is the sole usage-type catalog.**
   The gateway-local-from-config catalog is removed. There is one place where
   UsageTypes are declared, one place where they are looked up, and one place
   where they are deleted: the plugin's backend database, mutated via the
   gateway's SDK trait and REST surface, authorized by the Policy Decision
   Point (PDP) per ADR-0001.
2. **Usage records reference UsageTypes via `gts_id`.** The uuid5-from-type
   derivation is removed from the wire and from the storage schema. The
   `gts_id` string that identifies a UsageType in the catalog is the same value
   stored on every usage record that references it. No consumer or plugin
   author needs to re-implement UUID v5 derivation to validate or join.
3. **The UsageType specification no longer defines `parent_type_uuid`.** UsageType
   types are flat for v1; no parent pointer is carried on the catalog row, no
   inheritance chain is walked at validation time. If a future capability
   requires inheritance, it will be reintroduced by a dedicated ADR that names
   its consumer.
4. **The UsageType specification no longer defines `x-uc-indexable` or
   `abstract`.** Indexability hints and abstract-type markers do not appear on
   the UsageType specification surface. All UsageTypes registered in the
   catalog are concrete and queryable on their declared shape; indexing
   strategy is a plugin implementation concern.
5. **Counter/gauge semantics are derived from the `gts_id` prefix, not declared as a separate
   trait.** The catalog row carries no semantics column and the UsageType
   specification carries no semantics trait. The value (`counter` or `gauge`) is
   the leftmost `~`-separated base-type segment of the UsageType's `gts_id`. The
   two reserved base type identifiers are
   `gts.cf.core.usage.counter.v1~` and `gts.cf.core.usage.gauge.v1~`; every
   registered UsageType's `gts_id` MUST begin with exactly one of those two
   prefixes, and the counter-or-gauge semantics fall out deterministically from
   that prefix via `UsageTypeGtsId::is_counter()` / `UsageTypeGtsId::is_gauge()`.
6. **Closed `metadata_fields: Vec<String>` replaces open `metadata_schema`.**
   The catalog declares a closed list of metadata keys per UsageType. Only
   declared keys are accepted at ingest; there is no free-form remainder, no
   `additionalProperties: true` escape hatch, and no `extras` map. All values
   are typed as `String` at the SPI / validation layer (the catalog declares
   keys; values are conveyed as strings end-to-end). The Draft-07 JSON-Schema
   surface and the `jsonschema` runtime dependency are removed.

### Consequences

- The PRD (`PRD.md`), DESIGN (`DESIGN.md`), DECOMPOSITION (`DECOMPOSITION.md`),
  FEATUREs (`features/foundation.md`, `features/usage-type-lifecycle.md`,
  `features/usage-emission.md`), companion design docs (`domain-model.md`,
  `plugin-spi.md`, `sdk-trait.md`), and the OpenAPI YAML
  (`usage-collector-v1.yaml`) all carry references to the local-from-config
  catalog, to uuid5-derived UsageType identifiers, to `parent_type_uuid`, and to
  the dropped complexity attributes. Each of those artifacts must be revised
  in lockstep so the specification family describes one catalog model, one
  reference shape, and one attribute set.
- ADRs 0007, 0009, and 0010 are marked `superseded` with `superseded_by`
  pointing at this ADR. A one-line forward pointer is added to the body of
  each so a reader landing on those files immediately learns where the live
  decision lives. The superseded ADRs are not edited in their Decision or
  Consequences sections — they remain immutable beyond the status header and
  the forward pointer.
- ADR-0002 (`cpt-cf-usage-collector-adr-pluggable-storage`) is unchanged in
  its decision text; the catalog scope it carries was already re-expanded by
  ADR-0009 and remains so under this ADR.
- The single source of truth for the usage-type catalog simplifies operator
  mental model and downstream documentation: there is no longer a need to
  explain when the local catalog applies versus when the plugin catalog
  applies, or how the two are reconciled at boot.
- The `gts_id`-as-reference shape removes a class of bugs (consumers
  re-implementing UUID v5 derivation incorrectly) and removes one column from
  the storage row shape that downstream phases were going to define.
- The smaller spec and SDK surface aligns with the quota-reporting consumer
  narrowing in commit `783abdda`: v1 ships only what that consumer requires,
  with deferred complexity flagged as reserved for later ADRs that name a
  concrete consumer.
- Honest cost: `x-uc-indexable` as a documented hint is lost; downstream
  consumers that wanted to know which dimensions plugin authors should
  optimize for must now read the UsageType's metadata schema and decide
  per-backend. Honest cost: any external documentation, sample code, or
  partner integration that referenced the local-from-config catalog or the
  uuid5 derivation must be updated; an inventory pass is included in
  Phase 11's final consistency sweep.
- Migration mechanics from the prior model to the simplified model are a
  downstream cascade concern; this ADR does not specify migration ordering,
  which is owned by the DESIGN and FEATURE cascades (Phases 3, 5, 6).
- The 2026-06-02 amendment (simplifications 5 and 6) cascades through the
  same artifact family enumerated above — PRD, DESIGN, domain-model,
  plugin-spi, sdk-trait, DECOMPOSITION, features (`foundation.md`,
  `usage-type-lifecycle.md`, `usage-emission.md`), and the OpenAPI YAML
  (`usage-collector-v1.yaml`) — each of which must be revised in lockstep so
  that counter/gauge semantics are presented as derived from the `gts_id` prefix (no separate
  trait, no catalog column) and `metadata_schema` is replaced by closed
  `metadata_fields: Vec<String>` (declared keys only, all values typed as
  string).
- PRD §5.1 free-form-extras guarantee is dropped. The previously-promised
  "arbitrary-context extras" surface in PRD §5.1 is removed as an explicit
  consequence of the closed-`metadata_fields` simplification: undeclared keys
  are validation errors, not silently-preserved extras. Downstream cascade
  phases inherit this breakage and must rewrite PRD §5.1 in lockstep.
- The `jsonschema` runtime dependency that ADR-0010 introduced for the
  open-but-typed metadata schema is removed. Closed `metadata_fields` is
  validated by a small in-tree check (declared-keys membership + string
  type), so the gateway no longer needs a Draft-07 schema validator on
  the hot path. DECOMPOSITION drops the `jsonschema` crate dependency
  and the lift of the merge core (per ADR-0010 "Code reuse — lift, do
  not depend") in lockstep.
- The 2026-06-08 amendment (simplification 5' — kind moves from `gts_id` prefix to `UsageKind` enum; identifier landscape consolidated on `gts.cf.core.uc.*`) cascades through DESIGN, domain-model, plugin-spi, sdk-trait, DECOMPOSITION, features (`foundation.md`, `usage-type-lifecycle.md`, `usage-emission.md`), and the OpenAPI YAML (`usage-collector-v1.yaml`). Each artifact MUST be revised in lockstep so kind is presented as a closed `UsageKind` enum on the catalog row, every catalog `gts_id` derives from `gts.cf.core.uc.usage_record.v1~`, and the plugin GTS spec id reads `gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.plugin.v1~` everywhere.

### Amendment 2026-06-02 — kind from prefix and closed metadata_fields

This amendment extends the four original simplifications above (1-4) with two
further simplifications (5-6), without rewriting them. Status remains
`accepted`; ADR-0010 remains superseded and is not edited. The original four
bullets above are untouched.

**Rationale.** GTS guide §2.2 already encodes parent linkage in the `gts_id`
prefix as a `~`-separated, left-to-right inheritance chain (the leftmost
segment is the parent base type; the rightmost segment is the leaf), so a
separately-declared `kind` trait on the catalog row carries no information
the prefix does not already pin. Replacing the open Draft-07
`metadata_schema` with a closed `metadata_fields: Vec<String>` cuts the
`jsonschema` runtime dependency, simplifies validation to a declared-keys
membership check, and removes the open-extras attack surface (undeclared
keys are now validation errors instead of silently-preserved extras). Both
simplifications align the v1 surface with the quota-reporting consumer
narrowing pinned by commit `783abdda`.

**Pinned invariants (for downstream cascade phases to quote verbatim):**

- **Simplification 5 — counter/gauge semantics derived from `gts_id` prefix.** The catalog
  carries no semantics column and the UsageType specification declares no semantics
  trait. The counter-or-gauge value is derived from the leftmost
  `~`-separated base-type segment of a registered UsageType's `gts_id`. The two
  reserved base type identifiers are
  `gts.cf.core.usage.counter.v1~` and `gts.cf.core.usage.gauge.v1~`; every
  registered UsageType's `gts_id` MUST begin with exactly one of those two
  prefixes. A `gts_id` that does not begin with one of the two reserved
  prefixes is a registration validation error.
- **Simplification 6 — closed `metadata_fields` replaces open
  `metadata_schema`.** The catalog declares a closed list of metadata keys
  per UsageType as `metadata_fields: Vec<String>`. Only declared keys are
  accepted at ingest; undeclared keys are validation errors. There is no
  free-form remainder, no `additionalProperties: true` escape hatch, and no
  `extras` map. All values are typed as `String` at the SPI / validation
  layer (the catalog declares keys; values are conveyed as strings
  end-to-end). The Draft-07 JSON-Schema surface and the `jsonschema` runtime
  dependency are removed.

**Cascade scope.** This amendment cascades through PRD, DESIGN,
domain-model, plugin-spi, sdk-trait, DECOMPOSITION, features
(`foundation.md`, `usage-type-lifecycle.md`, `usage-emission.md`), and the
OpenAPI YAML (`usage-collector-v1.yaml`). The PRD §5.1 free-form-extras
guarantee is dropped as an explicit cost; downstream cascade phases inherit
this breakage and must rewrite PRD §5.1 in lockstep. Phases 2-9 of the
`update-usage-collector-flatten-metadata-and-kind-prefix` plan execute the
artifact-by-artifact cascade; Phase 10 performs the final consistency sweep.

### Amendment 2026-06-05 — unified UsageType and dropped created_at

This amendment extends the prior simplifications (1-6) with two further
simplifications (7-8). Status remains `accepted`; ADR-0010 remains
superseded and is not edited. The original bullets and the 2026-06-02
amendment are untouched.

**Rationale.** The catalog row, the register-input payload, and the
read / list response had drifted into five duplicate Rust types
(`UsageTypeRecord`, `RegisterUsageTypeInput`, `CreateUsageTypeRequest`,
`UsageTypeResponse`, plus the unused `UsageType` convenience facade)
that all carried the same two semantic fields (`gts_id`,
`metadata_fields`). Operator-visible `created_at` was an operator-audit
ergonomic — no flow branched on it and the list-ordering keyset is
already deterministic on `gts_id` alone (the PK). Collapsing the five
types into one canonical `UsageType` and dropping `created_at`
eliminates ~60% of the catalog type surface, removes the
register / response conversion boilerplate, and clarifies that there
is one shape, used everywhere.

**Pinned invariants (for downstream cascade to quote verbatim):**

- **Simplification 7 — single `UsageType` type across every catalog
  surface.** The Rust SDK declares one `UsageType { gts_id,
metadata_fields }`. The same type is the register-input payload,
  the register response, the read response, the list-page row, the
  SPI register / read / list input/output, and the REST request /
  response body. No `UsageTypeRecord`, `RegisterUsageTypeInput`,
  `CreateUsageTypeRequest`, or `UsageTypeResponse` exists.
- **Simplification 8 — `created_at` removed from the row shape.**
  The catalog row carries `gts_id` (PK) and `metadata_fields` only.
  The plugin does not stamp an accept timestamp and the
  `usage_type_catalog` table carries no `created_at` column. List
  pagination is keyed on `gts_id` ascending alone; the keyset is
  deterministic because `gts_id` is unique. The REST and SDK
  surfaces do not emit `created_at` on any response.

**Cascade scope.** This amendment cascades through DESIGN
(§3.7 stateless-gateway statement), domain-model, plugin-spi (Method 6 / 7 /
8 + the `CatalogRow` section + the `usage_type_catalog` columns
table), sdk-trait (UsageType entity + Method 6 / 7 / 8 + the
inputs/outputs table), DECOMPOSITION (catalog-row shape bullet), the
usage-type-lifecycle feature (every row-shape mention + the changelog),
and the OpenAPI YAML (the `UsageTypeRecord` and
`CreateUsageTypeRequest` schemas merge into a single `UsageType`
schema; `Page_UsageType.items` references it; the `created_at`
property is dropped). The wire shape is broken at the `v1` surface
because this is pre-release foundation work.

### Amendment 2026-06-08 — kind moves from gts_id prefix to UsageKind enum

This amendment supersedes simplification 5 of the 2026-06-02 amendment. Status remains `accepted`; the original four simplifications (1-4) and simplifications 6-8 of the 2026-06-02 / 2026-06-05 amendments are unaffected. Prior amendments are not edited.

**Rationale.** Simplification 5 encoded the counter / gauge discriminator into the `gts_id` chain root prefix and validated registration by prefix-match against a closed two-element set (`gts.cf.core.usage.counter.v1~` / `gts.cf.core.usage.gauge.v1~`). On review, that shape conflates type identity with classification axis: counter and gauge are CF-platform-internal closed-set classifications, not sibling GTS roots. `guidelines/GTS.md` §6.6 explicitly carves a closed-enum escape valve for discriminators that are closed, do not need descriptions, are not authz-relevant, are not vendor-extensible, and do not cross service-domain boundaries — all of which hold for counter / gauge in CF. The prefix-encoded shape also leaves three separate GTS namespaces (`usage`, `uc`, `usage_collector`) coexisting across catalog data, REST / PEP / error markers, and the plugin spec.

**Pinned invariants (for downstream cascade phases to quote verbatim):**

- **Simplification 5' — kind is a closed Rust enum on `UsageType`, not encoded in `gts_id`.** The SDK declares `UsageKind { Counter, Gauge }` (serde `rename_all = "lowercase"`) and `UsageType` carries a `kind: UsageKind` field. Catalog `gts_id`s no longer self-classify; the catalog row's `kind` column is the single source of truth.
- **Single `gts.cf.core.uc.usage_record.v1~` base type.** Every registered `UsageType`'s `gts_id` MUST derive from `gts.cf.core.uc.usage_record.v1~` with at least one further `~`-separated segment. The same identifier is the PEP / canonical-error resource marker for the ingestion REST surface.
- **`gts.cf.core.uc.usage_type.v1~`** is the PEP / canonical-error resource marker for the catalog REST surface (create / get / list / delete usage-type definitions).
- **Removed identifiers.** `gts.cf.core.usage.counter.v1~`, `gts.cf.core.usage.gauge.v1~`, and `gts.cf.core.usage.record.v1~` no longer exist anywhere in the gear. The "two reserved prefixes" registration rule is replaced with "`gts_id` MUST derive from `gts.cf.core.uc.usage_record.v1~`". The `UsageTypeGtsId::COUNTER_BASE` / `GAUGE_BASE` SDK constants are replaced with `USAGE_RECORD_BASE`.
- **Plugin id alignment.** The plugin GTS spec id is `gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.plugin.v1~` (code is already correct; the docs realign from `cf.core.usage_collector.plugin.v1~`).

**Cascade scope.** This amendment cascades through DESIGN (§3.1 domain model, §3.3 API contracts, §3.5 external dependencies, §3.10 consistency), domain-model, plugin-spi (Method 6 / 7 / 8 IO + plugin id), sdk-trait (`UsageType` shape + error taxonomy), DECOMPOSITION (catalog-row shape + plugin id), features (`foundation.md` plugin id + kind-from-row prose, `usage-type-lifecycle.md` register validation + examples, `usage-emission.md` compensation rule), and the OpenAPI YAML (`UsageType` schema gains `kind`; every `~counter.v1~` / `~gauge.v1~` example `gts_id` is rewritten; the two-reserved-prefixes prose blocks are removed). The wire shape change is non-backward-compatible at the v1 surface; the branch is pre-release foundation work and ADR-0012 amendment 5 already established that wire breakage at this stage is acceptable.

### Confirmation

Compliance is confirmed through (a) cross-artifact `cpt --json validate` PASS
across every modified usage-collector artifact at the end of Phase 11; (b) the
downstream phase handoffs (`out/phase-02-prd-impact.md` through
`out/phase-10-openapi-changes.md`) producing matching change summaries that
each cite the four simplifications above verbatim; (c) PR review on branch
`usage-collector/simplified-specs` once the full cascade lands, confirming
that no artifact still references the local-from-config catalog, the
uuid5-from-type derivation, `parent_type_uuid`, `x-uc-indexable`, or
`abstract`.

## Pros and Cons of the Options

### Keep both catalogs plus all current attributes

Preserve the gateway-local-from-config catalog (ADR-0007 / 0009) alongside
the plugin-DB catalog, keep usage records referencing UsageTypes via the
uuid5-derived UUID, and retain `parent_type_uuid`, `x-uc-indexable`, and
`abstract` on the UsageType specification unchanged.

- Good, because the option requires no specification changes and no cascade
  rework; the current state is preserved as-is.
- Good, because every attribute that exists has a documented rationale in its
  originating ADR (0007 / 0009 / 0010); the option keeps those rationales in
  force without further interpretation.
- Bad, because two catalog sources of truth remain in the specification —
  operators must reason about when each applies and how they reconcile, and
  documentation must explain both.
- Bad, because usage records continue to reference UsageTypes via a derived
  UUID, which every consumer that wants to join usage rows back to catalog
  rows must re-derive from the type id; mismatches between derivations are
  silent and corrupt downstream aggregation.
- Bad, because `parent_type_uuid`, `x-uc-indexable`, and `abstract` enlarge
  the spec and SDK surface without a v1 consumer named in commit `783abdda`'s
  narrowed scope; cost is paid speculatively against deferred capabilities.

### Plugin-DB catalog only with gts_id referencing and dropped attributes

The plugin-DB catalog (managed via SDK/REST) is the sole usage-type catalog;
usage records reference UsageTypes via `gts_id`; the UsageType specification drops
`parent_type_uuid`, `x-uc-indexable`, and `abstract`.

- Good, because there is exactly one place where UsageTypes exist: the plugin
  database, mutated via the gateway's SDK/REST surface under PDP
  authorization. Operator and consumer mental model collapses to one shape.
- Good, because the usage-record UsageType reference is the same `gts_id` string
  that appears on every catalog row; no derivation is needed at any consumer
  to join the two, and no class of "we derived the UUID slightly differently"
  bugs can occur.
- Good, because the specification surface shrinks to match the v1
  quota-reporting consumer narrowing in commit `783abdda`; v1 ships only what
  that consumer requires.
- Good, because the GTS schema identity (the `gts_id` already stored on every
  UsageType schema per the platform GTS pattern) is the natural reference
  key; the simplification aligns the wire shape with the platform invariant
  rather than papering over it with a derivation.
- Bad, because every artifact in the usage-collector specification family
  carries language about the local catalog, the uuid5 derivation, or the
  dropped attributes; the cascade rework across PRD, DESIGN, DECOMPOSITION,
  three FEATUREs, three companion docs, and the OpenAPI YAML is real work
  (Phases 2-10 of the plan).
- Bad, because `x-uc-indexable` as a documented hint to plugin authors is
  lost; plugin authors choosing an indexing strategy must derive their hint
  list from the UsageType's metadata schema rather than reading the hint
  directly off the spec.
- Bad, because ADRs 0007, 0009, and 0010 must be marked superseded, with
  status-header edits and forward pointers on each (this ADR's Phase 1
  artifact-edit work covers exactly that).
- Neutral, because future capabilities (inheritance, indexability hints,
  abstract markers) are not foreclosed — they may be reintroduced by a
  dedicated ADR that names its concrete consumer at the time of need.

### Keep the local-from-config catalog only

Remove the plugin-DB catalog surface, keep the gateway-local-from-config
catalog as the sole source, and retain or simplify the attributes around it.

- Good, because the gateway carries no plugin SPI catalog surface; plugin
  authors implement only the usage-record path.
- Good, because catalog mutation requires no PDP round-trip — it happens at
  boot from configuration files under whatever access controls the deploy
  environment provides.
- Bad, because runtime UsageType registration via SDK or REST disappears;
  downstream operators that depend on registering UsageTypes dynamically lose
  that capability, which contradicts the runtime-registration story already
  shipped in the public API and the SDK trait.
- Bad, because the catalog has no durable persistence beyond the
  configuration file; recovering from operator misconfiguration requires
  editing files and restarting, rather than calling a REST endpoint.
- Bad, because the option reintroduces exactly the ADR-0007 status quo that
  ADR-0009 already reverted on referential-integrity grounds; the underlying
  rationale for ADR-0009 has not changed and would re-apply.

## More Information

- Commit `783abdda docs(usage-collector): narrow downstream consumer to quota reporting` — narrows the v1 downstream consumer to quota reporting; the simplification adopted here matches the spec surface to that narrowed scope.
- Commit `03f177d9 docs(usage-collector): model UsageTypes as GTS schemas with typed per-usage-type dimensions` — pins UsageTypes as GTS Type Schemas with a `gts_id`; the `gts_id`-as-reference decision in this ADR sits directly on top of that modelling commit.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
