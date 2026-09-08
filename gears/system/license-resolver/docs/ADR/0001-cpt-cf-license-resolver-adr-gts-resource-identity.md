---
status: accepted
date: 2026-06-11
---

# ADR-0001: Subject and Resource Identity as GTS Type plus Optional Instance ID

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [(a) GTS type (required) + optional instance id](#a-gts-type-required--optional-instance-id)
  - [(b) Single `GtsInstanceId`](#b-single-gtsinstanceid)
  - [(c) Free-form / opaque string](#c-free-form--opaque-string)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-license-resolver-adr-gts-resource-identity`

## Context and Problem Statement

License resolver answers "is THIS resource granted to THIS subject?", so every check must carry a stable, unambiguous
identity for both sides. Checks come in two kinds. A **specific-instance** check targets one concrete object — a
**named** instance (a stable, human-meaningful name such as a specific feature) or an **opaque** instance (addressed by
a UUID, e.g. a content item). A **whole-type** check targets an entire class before any instance id exists — e.g.
gating a `POST` that creates a new resource of that type, where the id is not yet known. Subject and resource domain
types are heterogeneous and externally owned (a feature type is owned by the feature module, a user type by its
identity module, etc.). Within the licensing contract objects (`cpt-cf-license-resolver-adr-typed-licensing-contracts`),
how should the Subject and Resource identity fields represent the type and id so that both check kinds are expressible
and the object declares the contract its `metadata` is validated against?

## Decision Drivers

* Both check kinds must be expressible: a specific instance (named or UUID-addressed) and a whole type (no instance id
  exists yet, e.g. on `POST`).
* The type must always be present and referenceable — for both subject and resource — so the resolver can resolve the
  contract schema, telemetry can be dimensioned by it, and rules can target it.
* This is a contract consumed by other modules, so identity components should be constrained to valid GTS concepts, not
  arbitrary strings.
* What a backend *answers* for an id-less (whole-type) check is the backend's policy, not the contract's: the engine
  validates shape, the plugin decides the outcome.
* There is no listing/enumeration in scope, so no wildcard/set expressions are needed in the identity.

## Considered Options

* (a) **GTS type (required) + optional instance id** for both Subject and Resource: `type` is a GTS type id; `id` is an
  optional stable well-known name or UUID
* (b) A single GTS **instance** identifier (`GtsInstanceId`) always carrying type + id combined via `~`
* (c) A free-form / opaque string with no GTS structure

## Decision Outcome

Chosen option: **(a) GTS type (required) + optional instance id**. Inside the licensing Subject and Resource contract
objects, identity is carried as two fields: `type` — the GTS type id of the derived licensing contract the object
instantiates (e.g. `gts.cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~`), always present — and an optional
`id` — a stable well-known name (e.g. a named feature) or a UUID (e.g. a content item).
Without the `id` the check asks about the whole type; with it, about a specific instance. The engine validates the
shape (type present, id well-formed when present); how an id-less check is answered is the backend plugin's policy.

Option (b) was rejected because an instance identifier denotes exactly one concrete instance by definition (GTS rule:
types end with `~`, instances never do) — it cannot express a whole-type entitlement, which the `POST`-style use case
requires. Option (c) was rejected because dropping GTS structure loses the typed identity (nothing can validate the
type) and lets callers pass ambiguous identifiers across a shared contract.

### Consequences

* The licensing base types carry identity as two fields — required `type`, optional `id` — for both Subject and
  Resource. Code MAY provide helpers converting to/from a combined GTS instance identifier where convenient; that is an
  implementation detail, not part of the contract.
* The resolver resolves the contract schema from `type` and validates conformance (per
  `cpt-cf-license-resolver-adr-typed-licensing-contracts`), while which resources are licensable — and how an id-less
  check is answered — is owned by the backend licensing service. The domain type a contract projects is not carried in
  the payload.
* Telemetry is dimensioned by the contract type (bounded cardinality); the instance id is never
  used as a label.

### Confirmation

Confirmed by design and code review of the licensing base types (identity is a required GTS `type` plus an optional
`id` on both Subject and Resource), plus unit tests asserting that the id-absent and id-present forms both pass
validation and are forwarded to the backend unchanged, and that a missing `type` is rejected as a validation error.

## Pros and Cons of the Options

### (a) GTS type (required) + optional instance id

Two contract fields: the licensing contract GTS type, and an optional instance name/UUID.

* Good, because both check kinds are expressible — whole-type (id absent) and specific instance (id present) — without
  sentinel values or flags.
* Good, because the contract states intent directly: the type is always there to validate and dimension by; the id is
  present exactly when a concrete instance is meant.
* Good, because the type stays GTS-typed and registry-validatable — it is exactly what the resolver resolves the
  contract schema from — and the id stays a simple natural key (name or UUID).
* Neutral, because callers holding a combined GTS instance identifier split it into the two fields (a trivial helper).
* Bad, because a caller could omit the id where a specific instance was intended; the backend must decide what an
  id-less check means for it.

### (b) Single `GtsInstanceId`

One GTS instance identifier always carrying type and id via `~`.

* Good, because one value carries the whole identity for concrete-instance checks.
* Bad, because an instance identifier denotes one concrete instance by definition — a whole-type entitlement (id not
  yet known, e.g. on `POST`) is not expressible without overloading the notation with sentinels or flags.
* Bad, because callers and backends must parse the combined string to recover the type for validation and telemetry.

### (c) Free-form / opaque string

An unstructured string with no GTS typing.

* Good, because it imposes no schema.
* Bad, because it loses GTS validation entirely, so the registry cannot validate the type and externally-owned types are
  no longer referenced in a structured way.
* Bad, because ambiguity across a shared contract undermines correct routing and auditing.

## More Information

GTS guidelines `guidelines/GTS.md`: §2.1/§2.2 define the type identifiers (ending with `~`) used in the `type` field;
§2.3/§2.4 describe GTS instance identifiers — the combined notation considered and rejected as option (b); §6.7 shows
the carrier pattern this shape follows (a GTS type id field plus a generic payload, dispatched by type id). Declaring
which externally-owned domain type a contract projects is out of scope here and deferred to a later change. The
contract objects these fields live in are decided by
`cpt-cf-license-resolver-adr-typed-licensing-contracts`; the resolver's own types live under `gts.cf.core.lic.*`.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-license-resolver-fr-resource-identity` — defines resource identity as the contract GTS type (required) plus
  an optional instance id, which this ADR records as canonical.
* `cpt-cf-license-resolver-fr-subject-identity` — subject identity follows the same shape inside the Subject contract
  object.
* `cpt-cf-license-resolver-principle-gts-typed-resource-identity` — this ADR is the rationale for that DESIGN principle.
* `cpt-cf-license-resolver-constraint-gts-via-types-registry` — the contract type is what gets resolved and validated
  against the registry.
