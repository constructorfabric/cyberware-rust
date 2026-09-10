---
status: accepted
date: 2026-08-12
---

# Declarative GTS projection contracts for quota enforcement

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [The contract model](#the-contract-model)
  - [Projection contents](#projection-contents)
  - [Attribution and authorization](#attribution-and-authorization)
  - [Resolution, validation, and failure surface](#resolution-validation-and-failure-surface)
  - [Policy checking and bootstrap consistency](#policy-checking-and-bootstrap-consistency)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [(a) Opaque payload plus size cap only](#a-opaque-payload-plus-size-cap-only)
  - [(b) Registered and enforced projection contracts](#b-registered-and-enforced-projection-contracts)
  - [(c) Registered projections for documentation only](#c-registered-projections-for-documentation-only)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-quota-enforcement-adr-projection-contracts`

## Context and Problem Statement

Quota Enforcement (QE) currently describes an evaluation request as a metric, an amount, and
two JSON objects: caller-authored operation `metadata` and operator-authored `quota.metadata`.
A byte-size limit is the only structural contract. The platform therefore cannot enumerate
which attributes a metric owner supplies, and a Policy expression such as
`request.region in arbitration.regions` can silently miss because a key is absent,
misspelled, or of the wrong type. The request remains callable but is not governable.

The license-resolver precedent separates a Gear's domain model from a registered GTS
projection: an abstract core-owned base is refined by a derived, Gear-owned contract, and the
runtime value is validated against that contract. QE needs the same boundary while preserving
one shared counter across calling services and aligning with Usage Collector's caller-supplied
attribution model: the authenticated service supplies the logical tenant and subjects, and PDP
authorizes that tuple before QE evaluates it.

## Decision Drivers

* The registry must answer which owner projections and metrics exist without reading Gear
  source.
* Cross-service consumption of one shared counter must remain expressible.
* Malformed requests must fail closed before Engine dispatch, distinctly from a legitimate
  `Denied` decision.
* Attribute references in operator CEL must be checked when a Policy is saved, not fail
  silently at runtime.
* Registry availability and latency must not enter the p95 ≤ 100 ms evaluation path.
* Consumer operations are service-to-service and must authorize caller-supplied attribution
  against the authenticated service principal.
* Operation metadata and operator-authored arbitration constraints legitimately have different
  schemas and ownership.

## Considered Options

* (a) **Opaque payload plus size cap only** — retain the current unschematized metadata bags.
* (b) **Registered and enforced projection contracts** — derive concrete subject/resource
  projections from QE bases, validate request instances before Engine dispatch, and validate
  Quota arbitration constraints when Quotas are written.
* (c) **Registered projections for documentation only** — publish schemas but do not validate
  runtime values.

## Decision Outcome

Chosen option: **(b), registered and enforced projection contracts**.

### The contract model

QE owns four abstract (`x-gts-abstract`) base types — `gts.cf.core.qe.subj.v1~`,
`gts.cf.core.qe.res.v1~`, `gts.cf.core.qe.request.v1~`, and
`gts.cf.core.qe.constraint.v1~` — plus the concrete
scope-discriminator type `gts.cf.core.qe.scope.v1~`. Only concrete derived contracts
are instantiable. A representative subject projection is
`gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~`; an owner may publish both user- and
tenant-scope projections.

This replaces the illustrative `gts.cf.qe.subject.type.v1~cf.qe.subject.user.v1`, which has
three defects:

* its root is `cf.qe.*` rather than core-owned `cf.core.qe.*`;
* its last segment lacks the trailing `~`, making it an instance where QE needs a type;
* its derived segment is owned by QE rather than by the metric-owning Gear.

The decisions that follow:

* **The metric owner declares the projection — never each caller.** Any authorized caller may debit
  directly through QE using that projection; the owner publishes the contract but does not proxy
  calls. Caller-specific projections would fragment one shared budget into N counters.
* **The subject projection is the type half of the identity a Quota binds to.** Applicable-Quota
  lookup remains keyed by `(projection_type, subject_id, metric)`; several Quotas may share that
  key. The consumer supplies `subjects: [{kind, id}]`, where `kind` is a well-known QE scope
  instance and `id` is an opaque non-empty string. QE maps `(metric, kind)` through the
  catalogue's unique entry to obtain `projection_type`; callers do not select a projection.
* **The consumer supplies the complete logical attribution tuple.** Every attribution-based consumer request
  carries `tenant_id`; QE materializes the tenant-scope subject from that value and accepts any
  additional applicable subjects in `subjects`. Missing, duplicate, unknown, or unadmitted
  subject kinds fail before evaluation. Subject ids are never derived from
  `SecurityContext`; the PDP authorizes the supplied tenant/subject/metric tuple against the
  authenticated service principal.
* **Scope cascade still resolves every supplied applicable scope through the catalogue.** The
  owner's `user.v1~` and `tenant.v1~`, for example, reach the Engine together. At bootstrap, QE
  derives the authoritative `(metric, scope) -> projection` index from admitted-metric traits;
  the request cannot override that mapping.
* **One operation-level `metadata` object travels on the wire.** It is validated once against
  the owner's single request contract for the metric, rather than being repeated per subject
  projection. Bootstrap rejects a missing or duplicate metric request contract.
* **Callers pay an explicit attribution and mapping cost**, copying the logical tenant,
  subjects, resource, and operation metadata out of their domain objects. That DTO-style
  boundary decouples domain evolution from contract evolution. An owner must therefore design
  one request contract usable by every caller, and adding a required property is breaking for
  all of them.
* **Metric identity is unchanged.** Metrics stay registry-owned. Naming a metric beneath its
  owning Gear is a candidate for the open cross-gear namespace question, and Usage Collector
  may adopt the same model later, but neither is required here.

### Projection contents

Every consumer request that invokes subject-based evaluation (`debit`, `reserve`, each batch item, and preview) carries
one request envelope:

| Property | At runtime |
|----------|------------|
| `tenant_id` | Caller-supplied target tenant; PDP owner-tenant authorization is mandatory. |
| `subjects` | Additional caller-supplied `SubjectRef { kind: GtsInstanceId, id: String }` values. The tenant scope is materialized from `tenant_id`; ids are opaque and non-empty. |
| `metadata` | One operation-level object validated against the metric owner's request contract. Required and never defaulted. |
| `resource` | Optional concrete resource projection with `type`, optional `id`, and required `metadata`; descriptive only in P1. |

The subject contracts do not carry runtime subject ids or per-subject metadata. They declare only
the `(scope, admitted_metrics)` traits from which QE builds its catalogue. A separate concrete
contract derived from `gts.cf.core.qe.request.v1~` defines the metric's operation metadata once.
The resource base retains `type`, optional `id`, and `metadata`.

* **Operation `metadata` is required on the wire.** Omitting it is non-conforming and MUST NOT
  be defaulted to `{}`; callers send an empty object when the request contract declares no
  properties.
* **The trait surface is closed, and QE owns it.** Both trait-bearing bases declare
  `required` trait keys in `x-gts-traits-schema` and carry no `x-gts-traits` values of their
  own. That shape is valid because the bases are abstract: the GTS store type-checks any trait
  values an abstract type provides, but skips the required-trait completeness check for an
  abstract leaf, and a concrete descendant closes the required traits (`gts` OP#13). QE cannot
  use the alternative pattern of a `default` per trait, which
  `gts.cf.core.am.tenant_type.v1~` uses, because `scope` and `admitted_metrics` have no safe
  default. Both bases also set `additionalProperties: false` on the trait schema. The registry
  composes the effective trait schema as an `allOf` over every block in the derivation chain,
  so a closed base block rejects a new descendant trait key. An owner therefore declares the
  QE-owned traits and adds none of its own. An owner that restates the trait schema must
  restate every ancestor trait key, because a closed descendant block that drops one orphans
  it and fails validation.
* **Each subject projection declares its subject scope** as a required `scope` trait in the
  base's `x-gts-traits-schema`. Its value is a `GtsInstanceId` narrowed to
  `gts.cf.core.qe.scope.v1~*`; P1 defines the well-known instances
  `gts.cf.core.qe.scope.v1~cf.core.qe.user.v1` and
  `gts.cf.core.qe.scope.v1~cf.core.qe.tenant.v1`. Scope instances are identity-only
  discriminators; they do not name a `SecurityContext` accessor. QE reads the
  registry-validated effective trait and compares instance ids directly. Encoding scope in the
  type-id name segment would force string parsing, which `guidelines/GTS.md` conventions 14 and
  15 rule out.
* **Each subject projection declares its admitted metrics** through an inherited
  `x-gts-traits` value whose entries are `GtsInstanceId`s narrowed via `x-gts-ref` to the platform
  metric base. `x-gts-ref` is **pattern-level only** — it validates that the value is a
  well-formed GTS id under the declared prefix, and nothing more. Three checks it does *not*
  perform, all of which QE therefore owns:
  * that the referenced metric is registered — checked at bootstrap when the catalog is built;
  * that the referenced id is genuinely an *instance* of the metric base, since a narrowed
    `x-gts-ref` is a prefix match, not an instance-of check;
  * that debiting the metric is permitted — checked at Gateway ingress against the catalog.

  A request naming an unadmitted metric fails closed. The registry thereby becomes an
  inventory of which metrics are metered against which owner and scope. Narrowing the set is a
  breaking contract change.
* **Each metric has one request contract.** A concrete contract derived from
  `gts.cf.core.qe.request.v1~` declares required traits `metric: GtsInstanceId` and
  `constraint_contract: GtsTypeId`, narrowed to the platform metric base and
  `gts.cf.core.qe.constraint.v1~` respectively. It refines the one operation-level `metadata`
  object. QE builds a unique `metric -> request contract` index at bootstrap.
* **A metric is admitted by at most one projection per `(metric, scope)` pair, per
  deployment.** One owner may own several metrics and one projection may admit several, so the
  constraint binds the pair, not the projection or the owner. Two projections admitting the
  same metric at the same scope would make the applicable-Quota set ambiguous and fragment one
  budget into two counters. QE rejects that at bootstrap rather than resolving it by
  precedence, because a silent winner would make the debited counter depend on configuration
  order. Distinct scopes of one owner admitting the same metric is the intended arrangement —
  it is what makes scope cascade expressible.
* **The resource projection carries identity plus schematized properties only.** It completes
  the request contract for resource-aware selection but does not enter the counter key. A
  Quota on a named resource is expressible today through properties; whether resource becomes
  a counter-key axis remains an Open Question.
* **Operator-authored `quota.metadata` is the arbitration surface of a separate constraint
  contract family**, derived from `gts.cf.core.qe.constraint.v1~`. The metric owner publishes
  and versions one constraint contract attached through the request contract's
  `constraint_contract` trait; operators
  populate it at Quota create/update. The request schema MUST NOT be reused here — a Quota may
  carry `regions: string[]` against a request's `region: string`, plus arbitration fields such
  as `weight` with no request-side counterpart.
* **Contracts validate shape, not business meaning.** Semantic value rules and attribute
  interpretation stay with the Engine and Policy layer; QE core does not become an
  attribute-query engine. Contract documents and Policy configs must not carry secrets or
  sensitive runtime values.

### Attribution and authorization

* **The whole consumer surface is service-to-service only.** Debit, credit, rollback, lease
  operations, batch debit, consumer snapshot reads, and consumer `evaluate_preview` calls all
  require an authenticated service principal. End users do not call QE directly; a consuming
  product serves self-service views through its backend.
* **PDP authorizes the explicit attribution tuple.** Consumer `tenant_id`, subject refs,
  metric, and optional resource are caller-supplied and untrusted until PDP grants the
  authenticated service principal access to that tuple. QE never derives them from
  `SecurityContext` and fails closed on PDP unavailability. QE rejects malformed public
  request shape before the PDP call, but resolves no catalogue or contract information
  until authorization succeeds.
* **Management remains on the user plane.** Quota CRUD, Policy administration, manager/operator
  reads, and management previews retain explicit target identity in their DTOs and PDP scope.
* **There is no `caller_type` field.** The authenticated service identity is already intrinsic
  to `SecurityContext`; a second caller-declared identity would be redundant or spoofable.
* **A future derivation or attestation layer remains possible as a thin wrapper.** It may build
  the same raw attribution tuple from forwarded user context, or accept attested injection once
  hop authentication such as mTLS/SPIFFE exists, without changing QE's core API.

### Resolution, validation, and failure surface

Contracts are resolved and snapshotted **outside** the evaluation transaction:

* **Bootstrap** builds an immutable in-process `ProjectionContractCatalog` for the
  deployment's configured projections; Gateway validates evaluation requests against that local
  snapshot. `types-registry` remains authoritative; QE's catalogue is only a validated local
  snapshot. A new contract version becomes evaluable only when the configured catalogue includes
  it. P1 has no runtime refresh or breaking-version activation path.
* **`QuotaManagementService`** owns the `TypesRegistryClient` and a bounded LRU cache for
  metric, projection, and constraint-contract lookups. Quota creation validates the
  registry contract and requires the projection to be in the configured evaluation catalogue.
* **`PolicyService`** resolves the contracts a Policy references and snapshots their schema
  and version with the immutable Policy version.
* **Registration happens in `types-registry`.** QE gains no registration endpoint.

**`EvaluationOrchestrator` performs no live registry lookup, deliberately.** Four reasons, each
independently sufficient:

* The canonical pipeline — public request-shape checks → PDP authorization → catalogue/contract validation and
  mapping → idempotency lookup → locked applicable-Quota read → Policy lookup → Engine evaluation → Debit-Plan validation → mutation →
  idempotency/outbox persistence → commit — has no registry step today.
* The `ProjectionContractCatalog` is process-local and the `TypesRegistryClient` cache belongs
  to Quota CRUD, so there is no hot-path cache to reuse.
* Hot-path metric-mode enforcement already comes from storage errors (`MetricNotRegistered` /
  `MetricNotQuotaGated`).
* A registry call would put an external dependency under the latency budget and, because QE
  fails closed, couple evaluation availability to registry availability.

This is consistent with `cpt-cf-quota-enforcement-adr-metadata-snapshot-timing`: stored Quota
arbitration values are captured with the locked Quota row.

**Envelope validation.** Three families carry a validated `metadata` object:
`gts.cf.core.qe.request.v1~`, `gts.cf.core.qe.res.v1~`, and `gts.cf.core.qe.constraint.v1~`.
Each of those bases requires `type` and `metadata`, and the derived contract refines the
`metadata` subschema alone. QE wraps operation `metadata` and `quota.metadata` into the selected
`{type, metadata}` contract envelope. The optional resource already arrives as the complete
`{type, id?, metadata}` projection and is validated directly. In every case QE validates the
whole document against the resolved contract, never the inner `metadata` subschema alone,
because that path would skip the base's own `required` and `additionalProperties` rules. The
subject family declares traits only and carries no `metadata`, so the rule does not apply to it.

| Surface | Validation point | Rule |
|---------|------------------|------|
| `quota.metadata` | Quota create/update | Resolve the constraint contract attached to the metric request contract and validate once before persistence. Stored arbitration data is not revalidated during evaluation. |
| Consumer evaluation attribution, operation metadata, and optional resource | Gateway ingress of debit, reserve, preview, and each batch item | First reject malformed public request shape: missing required fields, wrong public container types, missing or empty ids, duplicate kinds, and repeated tenant scope. Next authorize the complete tuple through PDP. Only then require registered/admitted scope kinds, complete catalogue mapping, and request/resource schema conformance. |
| Consumer snapshot attribution | Gateway ingress of snapshot reads | First reject malformed public tenant/subject/metric filter shape. Next authorize the complete target through PDP. Only then map scope kinds through the catalogue; no operation metadata is required. |
| Direct-operation target | Gateway ingress of credit, rollback, commit, and release | Authorize the explicit tenant plus Quota, original-operation, or lease identity through PDP; reuse persisted subject attribution where required for idempotency. |

A request with malformed public request shape maps to `InvalidArgument` before PDP. For structurally valid input,
PDP denial maps to `PermissionDenied` before any catalogue or contract lookup. After authorization, an
unknown/unadmitted subject kind, schema mismatch, missing metadata, or inadmissible metric maps to
`InvalidArgument` / HTTP 400 with a stable field-level reason. Contract rejection returns a
platform-canonical error, never `Decision::Denied`; the two surfaces remain mutually exclusive.
A Quota/Policy referring to a contract that exists but is not usable in that lifecycle state
maps to `FailedPrecondition`. No new response envelope is introduced.

### Policy checking and bootstrap consistency

**At Policy create/update**, `PolicyService` hands the Engine validator the snapshotted request,
resource, and constraint schemas. The `cel` validator parses and type-checks references in the
stable `{request, resource, arbitration}` input, returning line/column diagnostics. It also
checks relationships per-contract
validation cannot prove:

* paired-key type disagreement;
* non-intersecting value domains, which make a Quota permanently inert;
* operator/cardinality mismatch — comparing `quota.regions: string[]` with
  `request.region: string` using `==` rather than membership.

**At bootstrap**, QE registers its own missing definitions — the abstract bases, the
scope-discriminator type, and its P1 well-known instances — through `TypesRegistryClient`;
registration is idempotent and touches only QE-owned definitions. Concrete owner projections
remain published by their owning Gears, and `types-registry` remains the authoritative source
throughout — QE's catalogue is only a validated local snapshot. Bootstrap then validates a
closed consistency set:

* every configured subject/resource projection and metric request contract is registered,
  concrete, and derived from its
  QE base;
* every admitted metric reference resolves to a registered instance of the metric base.
  `x-gts-ref` covers neither check, because it is pattern-level only;
* no two configured projections admit the same metric at the same declared scope;
* every admitted metric resolves to exactly one concrete request contract, and that contract's
  attached constraint contract is registered, concrete, and derived from the constraint base.

QE reads each projection's registry-validated effective `scope` trait and compares its
`GtsInstanceId` directly.

Any mismatch fails gear bootstrap. Owner projections outside the configured catalogue remain
discoverable, but P1 rejects Quota and Policy writes that reference them.

### Consequences

**Compatibility rule.** Adding an optional property is non-breaking. Remove / rename / retype /
narrow — including narrowing the admitted-metric set — requires a new contract version. Adding
a *required* property likewise requires a new version, because every existing caller populates
the owner's contract.

**Breaking-version replacement is not supported in P1.** The current projection remains active. A
replacement contract may be registered, but P1 rejects Quota and Policy writes that reference it and
rejects bootstrap if the configured catalogue would leave any active Quota or Policy on an
incompatible version. Safe activation requires a future transition mechanism that prevents
cross-version admission and preserves idempotency across projection versions; QE provides no
projection alias or Quota/counter migration verb in P1.

* Attribute mistakes become save-time or ingress errors instead of silent Quota-selection
  misses.
* Cross-service shared counters remain intact because the metric owner supplies the stable
  projection identity.
* Callers take on projection mapping and contract-version upgrades.
* Quota writes and Policy writes depend on registry availability; the evaluation hot path does
  not.
* Arbitration constraints remain a separate operator-owned instance surface, with snapshot timing
  unchanged.
* This is the inbound counterpart of the strict Engine boundary in
  `cpt-cf-quota-enforcement-adr-evaluation-engine`: Engines receive only validated inputs, and
  QE still validates their outputs before mutation.

### Confirmation

Confirmed by documentation review against the GTS conventions and the license-resolver
base/derived implementation, then by implementation tests when code lands: type/instance and
abstract-base checks; missing-metadata rejection; caller-supplied attribution and PDP denial;
admitted-metric rejection; Quota write validation without hot-path revalidation; Policy
pair-check diagnostics; bootstrap mismatch failure; and proof that two callers using one
owner's projection resolve the same shared applicable-Quota set.

## Pros and Cons of the Options

### (a) Opaque payload plus size cap only

* Good, because it adds no registry or projection work.
* Good, because callers may send arbitrary fields without contract versioning.
* Bad, because the metering surface cannot be discovered or reviewed from the registry.
* Bad, because key, type, and cardinality mismatches silently change selection behavior.
* Bad, because every Engine must improvise validation and diagnostics.

### (b) Registered and enforced projection contracts

* Good, because the request and operator surfaces are discoverable, versioned, and
  enforceable.
* Good, because owner-declared identity preserves one shared counter across calling services.
* Good, because Policy property references and pair compatibility are checked before
  activation.
* Good, because validation failures are uniform fail-closed errors ahead of Engine dispatch.
* Bad, because owners govern schemas and every caller maintains projection mappings.
* Bad, because Quota and Policy writes acquire a registry dependency and schema-cache
  lifecycle.

### (c) Registered projections for documentation only

* Good, because it provides a registry inventory with less runtime work than option (b).
* Bad, because nothing keeps the published schema and actual payload aligned.
* Bad, because it retains option (a)'s silent selection failures while presenting false
  contract confidence.
* Bad, because an unvalidated contract is not a contract.

## More Information

The contract model ships as validated JSON Schema files in [`docs/schemas/`](../schemas/), not
only as prose: the four abstract bases, the concrete scope-discriminator type with its two P1
well-known instances, and worked `cf.genai.llm_gateway` owner examples for all four contract
families under [`docs/schemas/examples/`](../schemas/examples/). The examples exercise the
decisions above: one owner publishing user- and tenant-scope projections that admit the same
metrics (scope cascade), one metric request contract per metric and each shared by both
projections, a constraint contract whose `regions: string[]` pairs with the request's scalar
`region` (the cel operator/cardinality check), a request contract that declares no properties
and therefore takes `{}` on the wire, and a constraint contract that carries only an
arbitration field with no request-side counterpart.

The naming, one-level derivation, traits, abstract types, and typed-id rules come from
[`guidelines/GTS.md`](../../../../../guidelines/GTS.md). The reference decisions are
[`license-resolver` ADR-0001](../../../license-resolver/docs/ADR/0001-cpt-cf-license-resolver-adr-gts-resource-identity.md)
and
[`license-resolver` ADR-0003](../../../license-resolver/docs/ADR/0003-cpt-cf-license-resolver-adr-typed-licensing-contracts.md).
The reference SDK in PR #4458 confirms abstract bases, required wire `metadata`, typed GTS id
fields, and `x-gts-ref` narrowing; QE intentionally differs by accepting a PDP-authorized
caller-supplied attribution tuple and keeping all registry access off its hot path.

license-resolver registers a contract through the module that wants enforcement for that
resource, and it holds no shared state — so it never had to separate the party that declares a
contract from the party that calls it. QE does: several services debit one counter, so the
declaring party is pinned to the metric owner and a metric is admitted at one scope by one
projection. That is an extension of the pattern to a stateful gear, not a departure from it.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses:

* `cpt-cf-quota-enforcement-fr-projection-contracts`
* `cpt-cf-quota-enforcement-fr-contract-validation`
* `cpt-cf-quota-enforcement-fr-subject-type-registry`
* `cpt-cf-quota-enforcement-fr-attribute-based-quota-selection`
* `cpt-cf-quota-enforcement-fr-quota-resolution-policy`
* `cpt-cf-quota-enforcement-principle-declarative-projection-contracts`
* `cpt-cf-quota-enforcement-principle-pdp-authorized-attribution`
* `cpt-cf-quota-enforcement-principle-strict-engine-boundary`
* `cpt-cf-quota-enforcement-adr-metadata-snapshot-timing`
* `cpt-cf-quota-enforcement-adr-evaluation-engine`
