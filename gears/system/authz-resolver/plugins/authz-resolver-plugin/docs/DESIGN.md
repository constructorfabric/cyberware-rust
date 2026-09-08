Created:  2026-08-27 by Constructor Fabric

# Technical Design — AuthZ Resolver Plugin

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-design-authz-resolver-plugin`

<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Principles & Constraints](#2-principles--constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Evaluation Pipeline](#31-evaluation-pipeline)
  - [3.2 Packaging, Registration & Lifecycle](#32-packaging-registration--lifecycle)
  - [3.3 GTS Type Validator](#33-gts-type-validator)
  - [3.4 Scope Enforcer](#34-scope-enforcer)
  - [3.5 Policy Evaluator](#35-policy-evaluator)
  - [3.6 Materialization & Constraint Generation](#36-materialization--constraint-generation)
  - [3.7 Capability Negotiation & Degradation](#37-capability-negotiation--degradation)
  - [3.8 Trusted System Actors](#38-trusted-system-actors)
  - [3.9 Audit](#39-audit)
  - [3.10 Caching](#310-caching)
  - [3.11 Configuration](#311-configuration)
  - [3.12 Deny Vocabulary](#312-deny-vocabulary)
  - [3.13 Metrics](#313-metrics)
  - [3.14 Internal & External Dependencies](#314-internal--external-dependencies)
- [4. Additional Context](#4-additional-context)
- [5. Traceability](#5-traceability)

<!-- /toc -->

> Requirements (WHAT and WHY) are in [PRD.md](./PRD.md). Requirement references use
> `cpt-cf-authz-plugin-fr-*` / `cpt-cf-authz-plugin-nfr-*` ids from that PRD.

## 1. Architecture Overview

### 1.1 Architectural Vision

The plugin is a stateless decision function with two in-memory caches. It owns no
database, no migrations, and no REST surface. Its inputs come from four in-process
clients; its output is a decision plus a constraint set.

```
                     ┌──────────────────────────┐
   PEP ──▶ gateway ──▶  AuthZ Resolver Plugin   │
                     │                          │
                     │  validate                │
                     │  type validator ─────────┼──▶ types-registry
                     │  scope enforcer          │
                     │  policy evaluator ───────┼──▶ RBAC
                     │  materialize ────────────┼──▶ tenant-resolver
                     │                  └───────┼──▶ resource-group
                     │  constraint generator    │
                     │  audit                   │
                     └──────────────────────────┘
```

The gateway selects the plugin by **payload vendor**, not by GTS identifier: it reads its
own `vendor` setting, collects registered plugin instances whose `vendor` field equals it,
and takes the lowest `priority`.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-authz-plugin-fr-request-validation`, `cpt-cf-authz-plugin-fr-validation-before-io` | Fixed-order pipeline that validates request shape before any client call, so a malformed request cannot cost a round-trip (§3.1) |
| `cpt-cf-authz-plugin-fr-registration`, `cpt-cf-authz-plugin-fr-vendor-no-default`, `cpt-cf-authz-plugin-fr-dependency-startup-failure` | `ClientHub` registration keyed by payload vendor with no default vendor, and startup failure when a declared dependency is absent (§3.2) |
| `cpt-cf-authz-plugin-fr-gts-type-validation`, `cpt-cf-authz-plugin-fr-validation-order`, `cpt-cf-authz-plugin-fr-type-cache-outage` | Cache-first type validator with three modes and subject-then-resource fail-fast ordering (§3.3) |
| `cpt-cf-authz-plugin-fr-empty-scope-denies`, `cpt-cf-authz-plugin-fr-action-scope-mapping`, `cpt-cf-authz-plugin-fr-scope-class-derivation`, `cpt-cf-authz-plugin-fr-scope-match-exactness`, `cpt-cf-authz-plugin-fr-wildcard-scope`, `cpt-cf-authz-plugin-fr-scope-not-a-grant` | Scope enforcer with an operator-configurable operation-to-scope map plus boundary-verb derivation for adapter-declared ids (§3.4) |
| `cpt-cf-authz-plugin-fr-subject-type-classification`, `cpt-cf-authz-plugin-fr-groups-not-subjects`, `cpt-cf-authz-plugin-fr-delegate-to-rbac`, `cpt-cf-authz-plugin-fr-rbac-failure-distinct`, `cpt-cf-authz-plugin-fr-scope-provenance` | Policy evaluator delegating the decision to RBAC, with a three-way outcome mapping and SDK-side provenance re-derivation of the granting scope (§3.5) |
| `cpt-cf-authz-plugin-fr-constraint-materialization`, `cpt-cf-authz-plugin-fr-group-tenant-pairing`, `cpt-cf-authz-plugin-fr-barrier-status`, `cpt-cf-authz-plugin-fr-supported-properties`, `cpt-cf-authz-plugin-fr-empty-materialization-denies` | One constraint shape per materialization variant, barrier- and status-aware traversal, and an empty materialization treated as its own deny (§3.6) |
| `cpt-cf-authz-plugin-fr-pushdown-predicate`, `cpt-cf-authz-plugin-fr-capability-degradation`, `cpt-cf-authz-plugin-fr-expansion-ceiling`, `cpt-cf-authz-plugin-fr-infeasibility-precedence` | Capability negotiation that emits a push-down predicate when the PEP declares support and degrades to identifier lists otherwise (§3.7) |
| `cpt-cf-authz-plugin-fr-trusted-actors`, `cpt-cf-authz-plugin-fr-trusted-actor-pairing`, `cpt-cf-authz-plugin-fr-trusted-actor-count-logged` | Empty-by-default trusted-actor set matched on both halves of `(subject_type, subject_id)`, with the configured count logged at startup (§3.8) |
| `cpt-cf-authz-plugin-fr-audit-record`, `cpt-cf-authz-plugin-fr-audit-redaction`, `cpt-cf-authz-plugin-fr-audit-disabled-by-default` | Audit emitter writing a bounded, redacted record at every decision exit, disabled by default (§3.9) |

#### NFR Allocation

| NFR ID | NFR Summary | Allocated To | Design Response | Verification Approach |
|--------|-------------|--------------|-----------------|----------------------|
| `cpt-cf-authz-plugin-nfr-latency-attribution` | Three separate latency histograms | Metrics instrument set | End-to-end, RBAC call, and hierarchy read are timed independently, so a regression is attributable to a dependency (§3.13) | Instrument assertions in the metrics tests |
| `cpt-cf-authz-plugin-nfr-hierarchy-cache` | Bounded size and TTL; one in-flight fetch per key | Hierarchy cache | Single-flight cache with configurable entry cap and TTL; failures are shared with waiters but never cached (§3.10) | Concurrency tests asserting one fetch per key |
| `cpt-cf-authz-plugin-nfr-fail-closed` | Zero configuration paths from failure to allow | Every component | Every error path yields a deny or a typed error; no setting inverts that (§2 Principles & Constraints) | Automated tests plus inspection of the configuration surface |
| `cpt-cf-authz-plugin-nfr-machine-readable-denies` | One deny-code namespace | Deny vocabulary | All codes live in `gts.cf.core.errors.err.v1~cf.authz.errors.<name>.v1` (§3.12) | Unit tests over the code constants |
| `cpt-cf-authz-plugin-nfr-strict-config` | Unknown keys rejected at startup | Configuration | Every section denies unknown fields and names the offending key (§3.11) | Config-parsing tests per section |
| `cpt-cf-authz-plugin-nfr-no-subject-in-logs` | No subject id below audit level | Audit emitter, tracing call sites | The subject identifier appears in the audit record only (§2 Principles & Constraints, §3.9) | Inspection plus tests over emitted fields |

#### Key ADRs

None recorded for this plugin — the design decisions are carried in §2 Principles & Constraints and §4 Additional Context.

### 1.3 Architecture Layers

- [x] `p3` - **ID**: `cpt-cf-authz-plugin-tech-stack`

| Layer | Responsibility | Technology |
|-------|---------------|------------|
| SDK | Plugin trait, request/decision/constraint models, deny codes | Rust traits + `ClientHub` registration (`authz-resolver-sdk`) |
| Gateway | Vendor selection by payload vendor, priority fallback, request delegation | Rust gear (`authz-resolver`) |
| Plugin (this gear) | Validation, scope enforcement, RBAC delegation, materialization, constraint generation, audit | Rust, two in-memory caches, four in-process clients |
| Observability | Latency attribution and decision counters | OpenTelemetry metrics |

The plugin owns no database, no migrations, and no REST surface, so this design has no
database-schema or deployment-topology view — its persistence and transport are entirely
those of the gateway that hosts it.

## 2. Principles & Constraints

### 2.1 Design Principles

#### Fail closed, everywhere

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-principle-fail-closed`

Every unresolved condition denies. There is no configuration
that converts a dependency failure into an allow, and no code path that widens a grant to
recover from an error.

#### Translate, do not re-decide

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-principle-translate-not-re-decide`

RBAC owns additive union and `not_permissions`. The plugin
maps the SDK request shape onto the RBAC request shape and maps the answer back. A second
matcher here would be a second place for the two to disagree.

#### Never truncate a constraint

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-principle-never-truncate`

An over-large identifier expansion denies with an
infeasibility code. Truncation would silently narrow or widen access depending on which
side of the predicate the list sits — the one failure mode nobody would notice.

#### A group predicate is never alone

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-principle-group-tenant-pairing`

Group constraints are AND-paired with their owning
tenant predicate in the same constraint, so a membership row crossing tenants cannot
authorize a resource outside the group's tenant.

#### No compiled-in identities

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-principle-no-compiled-in-identities`

Trusted actors, vendor, and every threshold come from
configuration. A hard-coded product identity would ship a bypass to installations that do
not run that product.

#### Nothing sensitive in logs

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-principle-no-sensitive-logs`

Debug logging omits subject identifiers; audit records
carry a constraint count and a fingerprint rather than the predicates.

### 2.2 Constraints

#### No storage of its own

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-constraint-stateless`

The plugin owns no database, no migrations, and no REST surface. Its only state is two
in-memory caches — hierarchy reads and GTS type lookups — both bounded and TTL-expiring
(§3.10). Anything it cannot derive from a dependency at request time, it cannot know.

#### Selection by payload vendor

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-constraint-vendor-selection`

The gateway selects a plugin by the `vendor` payload field, not by GTS identifier, taking
the lowest `priority` among matches (§3.2). A deployment may therefore ship several PDPs
and switch between them by configuration alone — which is the whole reason the decision
function is a plugin.

#### In-process dependencies only

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-constraint-in-process-deps`

All four dependencies are resolved from `ClientHub` at `init()` and called in-process; the
plugin opens no network connections of its own. A missing dependency is a startup error
(§3.2).

## 3. Technical Architecture

### 3.1 Evaluation Pipeline

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-component-evaluation-pipeline`

`evaluate()` is the single trait method, running ten steps in order:

| # | Step | On failure |
|---|------|-----------|
| 1 | Validate request shape | Deny `invalid_request.v1` before any downstream call |
| 2 | Validate GTS types | Mode-dependent (§3.3) |
| 3 | Enforce token scopes | Deny `scope_mismatch.v1` |
| 4 | Evaluate permissions via RBAC | Business deny, or infrastructure error |
| 5 | Validate assignment-scope provenance | Fail closed: internal error, with an audit record |
| 6 | Materialize the granting scope | Typed deny for reserved variants |
| 7 | Require a non-empty materialization | Deny `insufficient_permissions.v1` / `constraints_unavailable.v1` |
| 8 | Branch on whether constraints are required | Deny `constraints_unavailable.v1` |
| 9 | Generate constraints | Deny `expansion_infeasible.v1` / `unsupported_property.v1` |
| 10 | Emit audit record | Non-fatal |

Ordering carries meaning. Validation precedes every network call so a malformed request
costs nothing. Subject-type validation precedes resource-type validation so the second
lookup is skipped on the first failure. Scope enforcement precedes RBAC so a token that
cannot authorize the action never reaches the permission store.

Provenance validation (§3.5) sits between the RBAC answer and every use of it — before
the scope-type metric is recorded and before any hierarchy read — so an allow whose scope
its own assignments do not justify cannot widen into platform-root materialization.

Audit runs last and only for `Ok(_)`: an infrastructure error is a log, not an audit
record. Provenance rejection is the single exception. The malformed allow did reach the
decision pipeline, so a bounded fail-closed deny record is emitted before the typed
internal error is returned — auditable without pretending the request got a decision.

### 3.2 Packaging, Registration & Lifecycle

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-component-packaging-lifecycle`

| Property | Value |
|----------|-------|
| Crate | `cf-gears-authz-resolver-plugin` |
| Gear name | `authz-resolver-plugin` |
| Capabilities | none — the plugin has no database and no REST surface |
| Dependencies | `types_registry`, `authz_resolver`, `rbac`, `tenant_resolver`, `resource_group` |

`init()` resolves four clients from `ClientHub` — `RbacServiceClientV1`,
`TenantResolverClient`, `ResourceGroupReadHierarchy`, `TypesRegistryClient` — and a missing
one is a startup error naming both the contract and the gear expected to publish it.

Registration order is deliberate: the types-registry instance is registered **before** the
`ClientHub` entry, so a registry failure cannot leave an orphaned hub entry pointing at a
plugin the gateway will never find. The registered instance id is
`gts.cf.toolkit.plugins.plugin.v1~cf.core.authz_resolver.plugin.v1~cf.builtin.authz_resolver.plugin.v1`.

`vendor` has no default. An inherited default would make a gateway/plugin mismatch quiet:
the plugin would register successfully and the failure would surface much later, as
"no plugin instances found for vendor …" on the first authorization call.

### 3.3 GTS Type Validator

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-component-gts-type-validator`

Three modes, from `gts_validation.mode`:

| Mode | Unknown type | Registry outage |
|------|--------------|-----------------|
| `strict` (default) | Deny `unknown_resource_type.v1` | Error |
| `warn` | Warn, proceed | Error |
| `off` | Registry not consulted | — |

The outage column is deliberately NOT mode-dependent. `warn` exists to tolerate an
incomplete type **registration**, which is a per-type, self-correcting condition. A
registry that is **down** is neither: allowing there meant every request rode through
unvalidated for the whole outage, which is the one case the mode must not cover.

A bounded LRU sits in front of the registry, keyed by type id. **Known and unknown results
are cached; unavailability is not.** Caching an outage would let a few seconds of registry
downtime deny every subsequent request for a full TTL, turning a transient failure into a
sustained one.

### 3.4 Scope Enforcer

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-component-scope-enforcer`

Token scopes are a ceiling, not a grant. Three behaviours:

- **Empty scope set** → deny. Absence of scopes is not absence of restriction; the
  fail-open reading of an empty list is the classic way to hand out full access.
- **Wildcard scope** (configurable, default `*`) → pass without action mapping.
- **Otherwise** → resolve a scope class from three ordered sources and require one
  presented scope to satisfy it: a verbatim `operation_to_scope` hit, then a class derived
  from the operation id's boundary verbs, then `default_unmapped_scope`.

Matching is exact equality **or** the `<class>:` namespaced prefix. A bare prefix match
would let `reader` satisfy `read` — a scope named for a different purpose silently
authorizing this one.

The default mapping classes `get` and `list` as `read`. Without those entries they would
fall through to `default_unmapped_scope` (`write`), and a read-only token would be denied
its own GET.

#### Boundary-verb derivation

A verbatim map is a workable contract for the platform's own closed verb vocabulary, but
data-plane operation ids are declared by adapter manifests — `list_objects`,
`policy_read`, `signed_url_write` — and that set is open, unbounded, and not knowable when
the platform config is written. So an id the map does not name is classified from its
**first and last** `-`/`_` separated segment, which is where those ids put the effect
verb. Interior segments are ignored on purpose: reading them would let a destructive id be
talked down to `read` by a word in the middle of its own name. Two recognized boundaries
that disagree (`read_things_delete`) derive nothing.

The weak case is one boundary recognized and the other not, when the recognized one is a
read verb and the unrecognized one is the id's real effect: `read_replica_create` is
structurally identical to `list_access_keys`, so no rule over segment positions separates
them. What separates them is *recognizing* the mutating verb, so the mutating vocabulary
the soundness depends on lives in `MUTATING_BOUNDARY_VERBS` and the derivation reads it
directly — not through `operation_to_scope`, which an operator replaces wholesale. An
omitted mutating verb therefore still contradicts a read boundary, and the id still lands
on whatever `default_unmapped_scope` the deployment configured, in either direction.

`scope_class_source` (`mapped` / `derived` / `default`) is on both debug records, so an
operator can tell which of the three answered.

### 3.5 Policy Evaluator

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-component-policy-evaluator`

Two stateless translations — subject type to principal type, evaluation context to RBAC
scope — around one RBAC call.

Subject-type classification accepts two shapes, because both reach the plugin in real
deployments: raw identity-provider claim values (`user`, `service`, `service_principal`)
and GTS-tagged subject types, matched by the `subject_*` substring so vendor and version
segments do not matter. Anything else returns no classification, which callers turn into a
fail-closed deny. Groups are never direct subjects.

Outcome mapping is deliberately three-way: an RBAC allow continues the pipeline, an RBAC
deny becomes a business deny with `insufficient_permissions.v1`, and an RBAC transport
failure becomes a service-unavailable error. Collapsing the last two would make an outage
indistinguishable from a policy decision in every dashboard downstream.

#### Assignment-scope provenance

An RBAC allow carries both its aggregate scope and the role assignments that produced it,
and the plugin checks that the first follows from the second before using it. The check is
the SDK's own — `PermissionGranted::validate_scope_provenance` re-derives the aggregate
from the grants — so producer and consumer share one classifier instead of maintaining two.

It runs before the scope-type metric and before any hierarchy read, because the failure it
catches is a widening one: a producer bug, a stale row, or a partially corrupt payload can
present `Global` where no contributing assignment is root-scoped, and materializing that
reaches the platform root. Rejection is fail-closed — `authz_scope_provenance_rejection_total`
is incremented, a bounded deny record is audited, and the caller gets a typed internal
error rather than a decision.

Two things this is not. It is not an anti-forgery boundary: the RBAC answer arrives
in-process, and a future remote transport must authenticate and integrity-protect the whole
payload rather than lean on this. And it does not apply to the trusted in-process system
actor, whose empty-grant allow is constructed locally (§3.8) and is not backed by persisted
assignments at all.

### 3.6 Materialization & Constraint Generation

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-component-constraint-generator`

The granting scope resolves into a materialization, and each variant produces a concrete
constraint shape:

| Materialization | Constraint |
|-----------------|-----------|
| `TenantDirect` | Tenant predicate on the owner column |
| `TenantSubtree` | Tenant predicate over the expanded subtree |
| `TenantSubtreePushdown` | Push-down subtree predicate carrying root, barrier mode, and status filter |
| `GroupSubtree` | Group predicate AND-paired with its owning tenant predicate |
| `Combined` | Up to two OR-combined constraints — tenant side, plus the AND-paired group side |
| `Denied` | Typed business deny (reserved scope variants) |

Barrier mode governs subtree traversal: `respect` (default) stops at self-managed tenant
boundaries, `ignore` traverses through them.

Status filtering is deliberately asymmetric between the granted root and its descendants:

- **Descendants** are *not* status-clamped. An absent request filter means no filter at all
  — every status. Descendant lifecycle status is a business concern the owning gear enforces
  per operation, not an authz-scope clamp; clamping here hid suspended and deleted
  descendants from a caller's own lifecycle reads (suspend a tenant, then re-read it).
- **The granted root** is clamped to `[Active]` on the eager expansion path, independently
  of the descendant filter: a Suspended or Deleted granted root never enters the allow-set.

The push-down path carries only the descendant filter (§3.7).

Two safety checks run inside generation, in a fixed order: the expansion ceiling first,
then supported-property verification. When both could fire, `expansion_infeasible.v1`
surfaces over `unsupported_property.v1` — the operator can act on the ceiling, whereas the
property mismatch is a consequence of it.

An allowed scope that materializes to no accessible identifier is its own deny, taken
before the `require_constraints` branch: `insufficient_permissions.v1` when a tenant
subtree resolves to no tenants or a group scope to no member resources,
`constraints_unavailable.v1` for a `Combined` allow with both sides empty. Without that
step a PEP declaring `require_constraints=false` would receive an *unconstrained* allow for
a scope that grants access to nothing — the emptiest possible grant read as the widest one.

### 3.7 Capability Negotiation & Degradation

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-component-capability-negotiator`

A PEP declares what predicate classes it can evaluate: tenant hierarchy, group membership,
group hierarchy.

Declaring the tenant-hierarchy capability yields a push-down subtree predicate instead of
an expanded identifier list. That is not only cheaper — it is *more correct*: no resolver
round-trip, no expansion ceiling to hit, and no window in which a cached expansion is stale
relative to the hierarchy. The push-down predicate carries the same barrier mode and the
same descendant status filter the eager expansion would have applied.

One documented asymmetry remains: the PEP's `tenant_closure` subquery applies its single
status clause to the closure as a whole, root row included, so the push-down path cannot
status-exclude the granted root the way the eager path does (§3.6). A Suspended or Deleted
granted root therefore survives in a push-down constraint but not in an eager one. This is
accepted rather than latent: the operation the constraint feeds is still status-gated by the
owning gear, so the wider closure grants visibility, not action. Narrowing it would need a
second status clause in the push-down predicate contract, not a change here.

Without the capability, the plugin degrades to explicit identifier lists. It never emits a
predicate the PEP has not claimed it can evaluate, because an ignored predicate is an
unconstrained read.

### 3.8 Trusted System Actors

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-component-trusted-actors`

The widest bypass in the plugin: a matching `(subject_type, subject_id)` pair
short-circuits to Allow, skipping scope enforcement and subject-type classification.

The set is **empty by default** and configured per deployment. Both halves must match
within the same entry, and the subject identifier is the load-bearing half — it is minted
in-process and never issued to a token holder, so a forged subject type alone cannot ride
the bypass. The configured count is logged at startup so the bypass surface is visible
without reading configuration back.

Nothing is compiled in. A built-in identity would not be wrong for the platform it was
written for, but every other installation would inherit a bypass for an actor it does
not run.

A trusted actor's `subject_type` tag is a private in-process marker, not a registered GTS
type, so the type validator (§3.3) at step 2 would resolve it to "unknown" and — under
`strict` — deny the actor before the step-4 short-circuit was ever consulted. The
validator therefore skips the **subject** leg for a trusted actor, exactly as request
validation and the scope enforcer already do; `trusted_system_actors` and
`gts_validation.mode: strict` compose. The skip is keyed on the (`subject_id`,
`subject_type`) pair, so borrowing the tag onto another id buys nothing: the **resource**
leg is still validated, because a resource type is an ordinary registered type no matter
who is asking.

### 3.9 Audit

- [x] `p2` - **ID**: `cpt-cf-authz-plugin-component-audit-emitter`

One structured event per completed evaluation, allow or business deny, gated by
`audit.enabled` (**default on**). A PDP that decides without leaving an audit trail is a
missing operational control, not a quiet default, so the flag exists to turn the control
OFF deliberately — and `init()` logs a `warn` when a deployment does. The record goes to
the dedicated `cf-authz.audit` tracing target, so volume is routed or sampled at the
subscriber rather than by disabling the control. Infrastructure errors are excluded: they
are logs and metrics, not authorization decisions. A malformed request IS a decision — it
denies with `invalid_request.v1` — and is audited like any other deny.

Sensitive-data exclusion is **structural rather than procedural** — the audit record type
has no bearer-token field, and raw predicates never reach it. What it carries instead is a
constraint count and a short fingerprint of the constraint set, which is enough to
correlate two decisions or detect a change without reproducing what was authorized.

### 3.10 Caching

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-component-cache`

Hierarchy reads go through an LRU with TTL and singleflight coordination: concurrent
requests for the same key share one fetch, with the leader publishing to a latched watch
channel so a late subscriber still observes the result. Errors propagate to waiters but are
**not** cached — a failed flight leaves the key clean for the next attempt.

The cache stores heterogeneous hierarchy reads behind a tagged value enum, downcast at the
call site. Event-driven invalidation is configurable and reserved for the Event Broker
integration; until then TTL is the only invalidation.

### 3.11 Configuration

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-design-configuration`

```yaml
gears:
  authz-resolver-plugin:
    config:
      vendor: "constructorfabric"      # REQUIRED — must equal the gateway's vendor
      priority: 100
      trusted_system_actors: []        # empty = nothing bypasses
      cache:
        ttl_seconds: 60
        max_entries: 10000
        singleflight_enabled: true
        event_invalidation:
          enabled: false               # reserved for the Event Broker integration
      gts_validation:
        mode: strict                   # strict (default) | warn | off
        schema_registry_endpoint: ~    # optional override; unset uses the in-process client
      scope_enforcement:
        wildcard_scope: "*"
        default_unmapped_scope: write
        operation_to_scope:            # get/list default to the read class
          get: read
          list: read
      capability_degradation:
        max_expansion_ids: 10000
      audit:
        enabled: true                  # default; turning it off is logged at startup
```

Every section rejects unknown keys at startup, so `cach:` fails naming the field rather
than silently taking defaults.

### 3.12 Deny Vocabulary

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-design-deny-vocabulary`

- **Implements**: `cpt-cf-authz-plugin-interface-deny-codes`

All deny codes live in one namespace so a client can branch on the code without parsing
prose: `gts.cf.core.errors.err.v1~cf.authz.errors.<name>.v1`, with
`scope_mismatch`, `insufficient_permissions`, `unknown_resource_type`,
`unsupported_property`, `expansion_infeasible`, `constraints_unavailable`, and
`invalid_request`.

`invalid_request` is the caller's own fault — an unknown subject type, an empty or
wildcard action, a missing resource type, an unreadable subject tenant. It is a deny and
not an error on purpose: `AuthZResolverError` has only `NoPluginAvailable`,
`ServiceUnavailable` and `Internal`, so propagating a malformed request reached the PEP as
a 500-class `Internal` that PEPs retry and that pages on-call for a typo no retry can fix.
The `invalid_request` classification is unchanged on the metrics side — it simply moved
from `authz_evaluation_error_total{error_type}` onto
`authz_evaluation_deny_total{reason}`.

### 3.13 Metrics

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-design-metrics`

| Metric | Purpose |
|--------|---------|
| `authz_evaluation_duration_milliseconds` | End-to-end decision latency |
| `authz_rbac_query_duration_milliseconds` | RBAC call, isolated from the rest |
| `authz_hierarchy_query_duration_milliseconds` | Hierarchy reads, isolated |
| `authz_constraint_compilation_duration_milliseconds` | Constraint generation |
| `authz_evaluation_deny_total` / `authz_evaluation_error_total` | Business deny vs infrastructure failure, kept apart |
| `authz_fail_closed_total` | Denials caused by a failure rather than a policy |
| `authz_evaluation_cache_hit_ratio` | Hierarchy cache effectiveness |
| `authz_evaluation_by_scope_type_total` | Which scope shapes the deployment actually produces |
| `authz_scope_provenance_rejection_total` | RBAC allows refused because their scope did not follow from their assignments |
| `authz_capability_negotiation_total` | Push-down vs degraded paths |
| `authz_token_scope_narrowing_total` | How often scopes, not roles, are the binding constraint |
| `authz_barrier_mode_override_total` | Use of `ignore` |
| `authz_unsupported_property_total` | PEPs asking for predicates they cannot evaluate |

The split between deny and error is the load-bearing one: a rising deny count is a policy
signal, a rising error count is an outage, and one dashboard must never mask the other.

### 3.14 Internal & External Dependencies

Every dependency is an in-process SDK client resolved from `ClientHub`; the plugin makes no
outbound network calls of its own and talks to no external system directly.

| Dependency Gear | Interface Used | Purpose |
|-----------------|----------------|---------|
| `authz_resolver` | plugin registration | Hosts the plugin and selects it by payload vendor (§3.2) |
| `types_registry` | `types-registry-sdk` client | Resolve and validate subject and resource GTS types (§3.3) |
| `rbac` | `rbac-sdk` `RbacServiceClientV1` | The authorization decision itself, plus the grants backing its scope (§3.5) |
| `tenant_resolver` | `tenant-resolver-sdk` client | Tenant ancestry, barrier mode, and status for subtree materialization (§3.6) |
| `resource_group` | `resource-group-sdk` client | Group membership and group-owning tenant for group-scoped constraints (§3.6) |

**Dependency Rules** (per project conventions):

- No circular dependencies — the plugin consumes RBAC; RBAC never consumes the plugin
- All inter-gear communication goes through the SDK clients above, never internal types
- Only integration/adapter gears talk to external systems; this plugin talks to none
- `SecurityContext` is propagated across every in-process call

External dependency surface is limited to OpenTelemetry for metrics export (§3.13).

## 4. Additional Context

**Why a plugin rather than a gear.** The decision function is the part a deployment is
most likely to want to replace — a different policy engine, an external PDP, a
product-specific model. Making it a plugin behind a vendor-selected gateway means the
enforcement points never change when it does.

**Relationship to RBAC.** RBAC answers *what may this subject do*; this plugin answers
*and what may it see*. Keeping barrier and status semantics here rather than in RBAC's
scope walk is what lets RBAC keep a simple inheritance model with no per-assignment
opt-out, while a PEP still gets visibility rules that depend on tenant self-management.

**Relationship to enforcement.** The plugin cannot verify that a PEP applied its
constraints. That is a real gap and a deliberate one: verification would require the PDP to
see the PEP's query. The mitigation is the SDK's PEP helpers, so applying constraints is a
library call rather than something each service reimplements.

## 5. Traceability

| Design section | Requirements |
|----------------|--------------|
| §3.1 Evaluation Pipeline | `cpt-cf-authz-plugin-fr-request-validation`, `cpt-cf-authz-plugin-fr-validation-before-io` |
| §3.2 Packaging, Registration & Lifecycle | `cpt-cf-authz-plugin-fr-registration`, `cpt-cf-authz-plugin-fr-vendor-no-default`, `cpt-cf-authz-plugin-fr-dependency-startup-failure` |
| §3.3 GTS Type Validator | `cpt-cf-authz-plugin-fr-gts-type-validation`, `cpt-cf-authz-plugin-fr-validation-order`, `cpt-cf-authz-plugin-fr-type-cache-outage` |
| §3.4 Scope Enforcer | `cpt-cf-authz-plugin-fr-empty-scope-denies`, `cpt-cf-authz-plugin-fr-action-scope-mapping`, `cpt-cf-authz-plugin-fr-scope-class-derivation`, `cpt-cf-authz-plugin-fr-scope-match-exactness`, `cpt-cf-authz-plugin-fr-wildcard-scope`, `cpt-cf-authz-plugin-fr-scope-not-a-grant` |
| §3.5 Policy Evaluator | `cpt-cf-authz-plugin-fr-subject-type-classification`, `cpt-cf-authz-plugin-fr-groups-not-subjects`, `cpt-cf-authz-plugin-fr-delegate-to-rbac`, `cpt-cf-authz-plugin-fr-rbac-failure-distinct`, `cpt-cf-authz-plugin-fr-scope-provenance` |
| §3.6 Materialization & Constraint Generation | `cpt-cf-authz-plugin-fr-constraint-materialization`, `cpt-cf-authz-plugin-fr-group-tenant-pairing`, `cpt-cf-authz-plugin-fr-barrier-status`, `cpt-cf-authz-plugin-fr-supported-properties`, `cpt-cf-authz-plugin-fr-empty-materialization-denies` |
| §3.7 Capability Negotiation & Degradation | `cpt-cf-authz-plugin-fr-pushdown-predicate`, `cpt-cf-authz-plugin-fr-capability-degradation`, `cpt-cf-authz-plugin-fr-expansion-ceiling`, `cpt-cf-authz-plugin-fr-infeasibility-precedence` |
| §3.8 Trusted System Actors | `cpt-cf-authz-plugin-fr-trusted-actors`, `cpt-cf-authz-plugin-fr-trusted-actor-pairing`, `cpt-cf-authz-plugin-fr-trusted-actor-count-logged` |
| §3.9 Audit | `cpt-cf-authz-plugin-fr-audit-record`, `cpt-cf-authz-plugin-fr-audit-redaction`, `cpt-cf-authz-plugin-fr-audit-disabled-by-default` |
| §3.10 Caching | `cpt-cf-authz-plugin-nfr-hierarchy-cache` |
| §3.11 Configuration | `cpt-cf-authz-plugin-nfr-strict-config` |
| §3.12 Deny Vocabulary | `cpt-cf-authz-plugin-nfr-machine-readable-denies` |
| §3.13 Metrics | `cpt-cf-authz-plugin-nfr-latency-attribution` |
| §2 Principles & Constraints | `cpt-cf-authz-plugin-nfr-fail-closed`, `cpt-cf-authz-plugin-nfr-no-subject-in-logs` |

- **PRD**: [PRD.md](./PRD.md)
- **Plugin operations**: [README.md](../README.md)
- **Upstream role semantics**: [RBAC design](../../../../rbac/docs/DESIGN.md),
  [RBAC PRD](../../../../rbac/docs/PRD.md)
- **ADRs**: none recorded for this plugin
- **Features**: no feature specifications exist for this plugin yet
