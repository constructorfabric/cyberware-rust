Created:  2026-08-27 by Constructor Fabric

# PRD — AuthZ Resolver Plugin

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Gear-Specific Environment Constraints](#31-gear-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Request Validation](#51-request-validation)
  - [5.2 GTS Type Validation](#52-gts-type-validation)
  - [5.3 Token Scope Enforcement](#53-token-scope-enforcement)
  - [5.4 Permission Evaluation](#54-permission-evaluation)
  - [5.5 Constraint Generation](#55-constraint-generation)
  - [5.6 Capability Negotiation & Degradation](#56-capability-negotiation--degradation)
  - [5.7 Trusted System Actors](#57-trusted-system-actors)
  - [5.8 Plugin Discovery & Registration](#58-plugin-discovery--registration)
  - [5.9 Audit](#59-audit)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

> **Abbreviations**: PDP = Policy Decision Point, PEP = Policy Enforcement Point,
> AuthZ = Authorization, GTS = Global Type System. Used throughout this document.

## 1. Overview

### 1.1 Purpose

The AuthZ Resolver Plugin is the platform's policy decision point. It answers one
question — *may this subject perform this action on this resource?* — and, when the answer
is yes, returns the constraints an enforcement point must apply to scope the data it reads.

It is a **plugin**, not a gear with its own domain: the AuthZ Resolver gateway selects it at
runtime, and every fact it decides on comes from somewhere else — roles from RBAC, tenant
ancestry from the Tenant Resolver, group closures from the Resource Group gear, type
validity from the types-registry. Its own contribution is the composition: turning those
facts into a decision plus a constraint set, fail-closed, within a latency budget a request
path can absorb.

### 1.2 Background / Problem Statement

A permission check alone does not make a multi-tenant listing endpoint safe. "Alice may read
virtual machines" is true and useless: the endpoint still has to know *which* virtual
machines. Without a constraint contract, every enforcement point re-derives tenant and group
scoping from raw role data, and each one gets it subtly wrong in its own way.

This plugin exists so that derivation happens once. A PEP receives a decision and a set of
predicates it can push into its own query, and the rules for building those predicates —
tenant subtree expansion, barrier handling, group closure, the AND-pairing that keeps a
group predicate inside its owning tenant — live in one component with one test suite.

### 1.3 Goals (Business Outcomes)

- Every management-plane authorization decision is produced by one component, so a policy
  change lands in one place
- PEPs receive push-down predicates and never re-derive scoping from role data
- A missing dependency or an unknown type denies; it never degrades into a broader grant
- A deployment can replace this plugin with its own PDP without touching enforcement points

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Decision | Allow or deny for one `(subject, action, resource)` triple |
| Constraint | A predicate a PEP applies to its own query so results stay inside the subject's authorization (`owner_tenant_id IN (…)`, `id IN (…)`, `InTenantSubtree(root)`) |
| Capability | A declaration by the PEP that it can evaluate a given push-down predicate class |
| Barrier | A self-managed tenant boundary that a subtree walk stops at unless told otherwise |
| Materialization | The resolved form of a grant's scope — a tenant, a tenant subtree, a group closure, or a push-down root |
| Trusted system actor | An in-process identity that short-circuits evaluation to Allow, configured explicitly and never inferred |
| Business deny | A denial produced by policy — scopes, roles, or an unknown type — as opposed to an infrastructure error |

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level by the steering committee.
> This section documents the actors that interact with the plugin.

### 2.1 Human Actors

#### Platform Operator

**ID**: `cpt-cf-authz-plugin-actor-platform-operator`

- **Role**: Configures which vendor's plugin the gateway selects, the cache and validation
  modes, the expansion ceiling, and which system actors are trusted.
- **Needs**: Configuration that fails at startup rather than on the first authorization
  call, and a visible record of every bypass the deployment grants.

#### Service Developer

**ID**: `cpt-cf-authz-plugin-actor-service-developer`

- **Role**: Implements a PEP against the returned constraint vocabulary.
- **Needs**: A predicate set the service can push into its own query, and a machine-readable
  reason whenever the answer is deny.

### 2.2 System Actors

#### AuthZ Resolver Gateway

**ID**: `cpt-cf-authz-plugin-actor-gateway`

- **Role**: Selects this plugin by payload vendor and routes evaluation requests to it.

#### Policy Enforcement Points

**ID**: `cpt-cf-authz-plugin-actor-pep`

- **Role**: Call the gateway and apply the returned constraints to their own queries. They
  also declare which push-down predicate classes they can evaluate.

#### RBAC Gear

**ID**: `cpt-cf-authz-plugin-actor-rbac`

- **Role**: Authoritative source of roles, permissions, and role-local denies. See the
  [RBAC PRD](../../../../rbac/docs/PRD.md).

#### Tenant Resolver

**ID**: `cpt-cf-authz-plugin-actor-tenant-resolver`

- **Role**: Tenant ancestry, subtree expansion, barrier and status filtering.

#### Resource Group Gear

**ID**: `cpt-cf-authz-plugin-actor-resource-group`

- **Role**: Group membership and closure resolution.

#### Types Registry

**ID**: `cpt-cf-authz-plugin-actor-types-registry`

- **Role**: Validity of the GTS types named in a request, and the plugin's own instance
  registration.

## 3. Operational Concept & Environment

Runtime, OS, and lifecycle policy are defined once at the repository level — see the
[architecture manifest](../../../../../../docs/ARCHITECTURE_MANIFEST.md) and the
foundational [guidelines/](../../../../../../guidelines/). The authorization-wide context
this plugin sits in is
[docs/arch/authorization/DESIGN.md](../../../../../../docs/arch/authorization/DESIGN.md).
Only plugin-specific deviations are recorded below.

### 3.1 Gear-Specific Environment Constraints

- **No database and no persistent state.** The plugin runs in-process inside a gear host,
  registers itself in the types-registry at `init()`, and publishes an in-process client
  into `ClientHub`. Its only state is two in-memory caches — hierarchy reads and GTS type
  lookups — both bounded and TTL-expiring.
- **Selection is by payload vendor, not by GTS identifier.** The gateway reads its own
  `vendor` setting, collects the registered plugin instances whose `vendor` field equals it,
  and takes the lowest `priority`. A deployment can therefore ship several PDPs and switch
  between them by configuration alone.

## 4. Scope

### 4.1 In Scope

- Evaluating one `(subject, action, resource, context)` request into a decision
- Enforcing token scopes as a ceiling on the decision
- Validating the GTS types named in a request, in a configurable mode
- Generating tenant, group, and combined constraints, with barrier and status filtering
- Negotiating push-down predicates against PEP-declared capabilities, and degrading to
  explicit identifier lists when they are not declared
- Bounding expansion size and reporting infeasibility rather than truncating
- Emitting a structured audit record per completed evaluation
- Trusting an explicitly configured set of in-process system actors

### 4.2 Out of Scope

- **Authentication.** The plugin consumes an already-authenticated security context.
- **Role administration.** Roles and assignments belong to
  [RBAC](../../../../rbac/docs/PRD.md); this plugin only reads.
- **Enforcement.** Applying constraints is the PEP's responsibility; the plugin cannot
  observe whether they were applied.
- **Reinterpreting RBAC semantics.** Additive union and `not_permissions` are decided by
  RBAC; the plugin is a translator, not a second matcher.
- **Persistence.** No decision log, and no stored state beyond the in-memory caches.
- **Decision caching.** Only hierarchy and type lookups are cached; caching decisions is
  deferred until invalidation is defined.

## 5. Functional Requirements

> **Testing strategy**: All requirements are verified via automated tests (unit,
> integration, e2e) targeting 90%+ coverage unless otherwise specified. Verification method
> is documented only where a non-test approach applies.

### 5.1 Request Validation

#### Structural Validation

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-request-validation`

The plugin **MUST** reject a structurally invalid request — missing subject type, missing
action, missing resource type — before any downstream call.

- **Rationale**: A malformed request cannot be decided, and finding that out after four
  round trips wastes the request path's entire latency budget.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`

#### Validation Before I/O

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-validation-before-io`

Validation failures **MUST NOT** consume RBAC or hierarchy round trips.

- **Rationale**: Ordering validation ahead of every network call is what makes a malformed
  request cost nothing, including under a flood of them.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`

### 5.2 GTS Type Validation

#### Type Validation Modes

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-gts-type-validation`

The plugin **MUST** validate the subject type and the resource type named in a request
against the types-registry in one of three modes: `strict` (default) — an unknown type
denies; `warn` — an unknown type is logged and proceeds; `off` — the registry is not
consulted. A registry **outage** is not mode-dependent: it fails closed in both `strict`
and `warn`.

- **Rationale**: Deployments differ in how complete their type registration is, so the
  mode is configurable rather than hard-coded. The DEFAULT is nonetheless the closed one:
  a PDP that cannot confirm the type it is deciding about is degraded, and `warn` exists
  as an explicit, logged opt-out for a rollout that is still registering its types — not
  as the posture an unconfigured deployment inherits. Tolerating an incomplete
  registration is a different question from tolerating a registry that is down, which is
  why the outage row is the same in both modes.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`, `cpt-cf-authz-plugin-actor-types-registry`

#### Validation Order

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-validation-order`

Subject type **MUST** be validated before resource type, and the first failure **MUST**
short-circuit the second lookup.

- **Rationale**: Two registry lookups where one answer already decides the request is one
  lookup too many on the hot path.
- **Actors**: `cpt-cf-authz-plugin-actor-types-registry`

#### Outage Is Never Cached

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-type-cache-outage`

Known and unknown lookup results are cached; registry **unavailability** **MUST NOT** be
cached.

- **Rationale**: Caching an outage would let a few seconds of registry downtime deny every
  subsequent request for a full TTL, turning a transient failure into a sustained one.
- **Actors**: `cpt-cf-authz-plugin-actor-types-registry`

### 5.3 Token Scope Enforcement

#### Empty Scope Set Denies

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-empty-scope-denies`

An empty token-scope set **MUST** deny.

- **Rationale**: Absence of scopes is not absence of restriction. The fail-open reading of
  an empty list is the classic way to hand out full access.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`

#### Action-to-Scope Mapping

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-action-scope-mapping`

The plugin **MUST** resolve the requested action to a scope class from three ordered
sources — a verbatim configuration entry, then a class derived from the action id's boundary
verbs, then a configured default for unmapped actions — and **MUST** require at least one
presented scope to satisfy that class.

- **Rationale**: Token vocabularies are issued by the identity layer, not by this plugin, so
  the mapping has to be configuration. The default for unmapped actions is the write class,
  so a new action is restricted until somebody classifies it. Verbatim entries come first so
  an operator can always pin one action id and take the derivation out of the play.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

#### Derivation Refuses Rather Than Guesses

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-scope-class-derivation`

Deriving a scope class **MUST** consider only the action id's first and last separated
segments, **MUST** derive nothing when two recognized boundaries disagree, and **MUST**
recognize the mutating verb vocabulary independently of the operator-supplied mapping.

- **Rationale**: Data-plane action ids are declared by adapter manifests, so the set is open
  and cannot be enumerated in platform config; the effect verb sits at a boundary, and
  reading interior segments would let a destructive id be talked down to `read` by a word in
  the middle of its own name. Recognizing the mutating verbs independently is what makes the
  weak case safe: an operator map that omits `create` must still not let
  `read_replica_create` derive the `read` at its other boundary unopposed.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`, `cpt-cf-authz-plugin-actor-pep`

#### Exact or Namespaced Scope Matching

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-scope-match-exactness`

Scope matching **MUST** be exact equality or a namespaced prefix (`<class>:`), never a bare
prefix.

- **Rationale**: A bare prefix match would let `reader` satisfy `read` — a scope named for a
  different purpose silently authorizing this one.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`

#### Wildcard Scope

- [x] `p2` - **ID**: `cpt-cf-authz-plugin-fr-wildcard-scope`

A configured wildcard scope **MUST** pass the check without action mapping.

- **Rationale**: A token deliberately issued for full access should not require every action
  to be enumerated in the mapping first.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

#### Scopes Are a Ceiling, Not a Grant

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-scope-not-a-grant`

Passing the scope check **MUST NOT** imply a grant: RBAC remains authoritative and may still
deny.

- **Rationale**: Scopes narrow what a token may attempt; roles decide what the subject may
  do. Collapsing the two would let a broadly scoped token bypass the role model.
- **Actors**: `cpt-cf-authz-plugin-actor-rbac`

### 5.4 Permission Evaluation

#### Subject-Type Classification

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-subject-type-classification`

The plugin **MUST** resolve the subject's principal type from the presented subject type,
supporting both raw identity-provider claim values and GTS-tagged subject types, and **MUST**
deny any value it cannot classify.

- **Rationale**: Both shapes reach the plugin in real deployments. Denying the unclassifiable
  case is the fail-closed direction: an unrecognised subject type must not be evaluated as
  some default principal kind.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`, `cpt-cf-authz-plugin-actor-rbac`

#### Groups Are Not Subjects

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-groups-not-subjects`

Groups **MUST NOT** be accepted as direct subjects.

- **Rationale**: A group is a way of holding a grant, not a caller. Accepting one as the
  subject would authorize a request nobody made.
- **Actors**: `cpt-cf-authz-plugin-actor-rbac`

#### Delegate the Permission Decision

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-delegate-to-rbac`

The plugin **MUST** delegate the permission decision to RBAC and **MUST NOT** re-evaluate
its rules.

- **Rationale**: A second matcher here would be a second place for the two components to
  disagree, and disagreement in authorization is indistinguishable from a bug in either.
- **Actors**: `cpt-cf-authz-plugin-actor-rbac`

#### An Allowed Scope Must Follow From Its Assignments

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-scope-provenance`

For a normal RBAC allow the plugin **MUST** verify that the aggregate scope follows from the
contributing role assignments, **MUST** do so before recording the allow or performing any
hierarchy read, and **MUST** fail closed when it does not.

- **Rationale**: The allow carries both the scope and the assignments that produced it, and
  nothing else ties them together. A producer bug, a stale row, or a partially corrupt
  payload can present a root-level aggregate that no contributing assignment justifies, and
  materializing it reaches the platform root. Verifying before the first hierarchy read is
  what keeps that from becoming a widening. The already-authenticated in-process system
  actor is exempt: its allow is constructed locally and is not backed by persisted
  assignments.
- **Actors**: `cpt-cf-authz-plugin-actor-rbac`, `cpt-cf-authz-plugin-actor-platform-operator`

#### Transport Failure Is Not a Deny

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-rbac-failure-distinct`

An RBAC transport failure **MUST** surface as an infrastructure error, distinct from a
business deny.

- **Rationale**: Collapsing the two would make an outage indistinguishable from a policy
  decision in every dashboard downstream — a rising deny count would no longer mean anything.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

### 5.5 Constraint Generation

#### Constraint Materialization

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-constraint-materialization`

For every allowed decision the plugin **MUST** materialize the granting scope into
constraints: a tenant predicate for tenant-scoped grants, a group predicate for group-scoped
grants, and an OR-combination when both apply.

- **Rationale**: A decision without constraints is unusable for a listing endpoint, which is
  the case that motivated the plugin.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`

#### An Empty Materialization Is a Deny

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-empty-materialization-denies`

An allowed scope that materializes to no accessible tenant or resource identifier **MUST**
deny, including when the PEP declared that it does not require constraints.

- **Rationale**: Otherwise the emptiest possible grant is served as the widest possible
  answer — a decision-only allow with no constraints attached, for a scope that grants
  access to nothing.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`

#### A Group Predicate Is Never Alone

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-group-tenant-pairing`

A group predicate **MUST** always be AND-paired with the owning tenant predicate in the same
constraint.

- **Rationale**: A membership row that crosses tenants would otherwise authorize a resource
  outside the group's tenant — a cross-tenant read produced by a correct-looking predicate.
- **Actors**: `cpt-cf-authz-plugin-actor-resource-group`

#### Barrier Mode and Status Filtering

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-barrier-status`

Subtree materialization **MUST** respect the request's barrier mode — `respect` (default)
stops at self-managed tenant boundaries, `ignore` traverses through them.

Tenant status filtering is split by role in the grant:

- A granted root tenant that is not Active **MUST NOT** enter an eagerly expanded allow-set.
- Descendant tenants **MUST NOT** be status-clamped when the request carries no status
  filter: an absent filter means every status. Descendant status is enforced per operation by
  the owning gear, and clamping it here removed a caller's ability to read the lifecycle
  state of its own suspended or deleted descendants.
- A push-down predicate **MUST** carry the descendant filter. It is **NOT** required to
  status-exclude the granted root, because the predicate contract has one status clause for
  the whole closure; the resulting closure is wider by at most the granted root, and the
  operation it feeds remains status-gated by the owning gear.

- **Rationale**: Barrier and status semantics are applied here, during constraint
  generation, rather than by RBAC's scope inheritance. That split is what lets RBAC keep an
  inheritance model with no per-assignment opt-out while a PEP still gets visibility rules
  that depend on tenant self-management. See
  [RBAC `cpt-cf-rbac-fr-scope-taxonomy`](../../../../rbac/docs/PRD.md#54-scope-inheritance).
- **Actors**: `cpt-cf-authz-plugin-actor-tenant-resolver`, `cpt-cf-authz-plugin-actor-rbac`

#### Only Supported Properties Are Emitted

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-supported-properties`

Every emitted predicate's property **MUST** appear in the PEP's declared supported
properties; otherwise the plugin **MUST** deny with a distinct code rather than emit a
predicate the PEP will ignore.

- **Rationale**: An ignored predicate is an unconstrained read. Denying is the only safe
  outcome, and a distinct code tells the operator which side to fix.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`

### 5.6 Capability Negotiation & Degradation

#### Push-Down Subtree Predicate

- [x] `p2` - **ID**: `cpt-cf-authz-plugin-fr-pushdown-predicate`

When the PEP declares the tenant-hierarchy capability, the plugin **MUST** emit a push-down
subtree predicate instead of an expanded identifier list.

- **Rationale**: Not only cheaper but *more correct*: no resolver round trip, no expansion
  ceiling to hit, and no window in which a cached expansion is stale relative to the
  hierarchy. The push-down predicate carries the same barrier mode and status filter the
  eager expansion would have applied, so the two are equivalent.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`

#### Degradation to Identifier Lists

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-capability-degradation`

When a capability is not declared, the plugin **MUST** degrade to explicit identifier lists
rather than emitting a predicate the PEP cannot evaluate.

- **Rationale**: The plugin cannot verify enforcement, so it must never rely on a capability
  the PEP has not claimed.
- **Actors**: `cpt-cf-authz-plugin-actor-pep`

#### Expansion Ceiling

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-expansion-ceiling`

An expansion that exceeds the configured ceiling **MUST** deny with an infeasibility code,
and truncating an identifier list is **forbidden**.

- **Rationale**: Truncation would silently narrow or widen access depending on which side of
  the predicate the list sits — the one failure mode nobody would notice.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

#### Infeasibility Outranks Unsupported Property

- [x] `p2` - **ID**: `cpt-cf-authz-plugin-fr-infeasibility-precedence`

When both the expansion ceiling and an unsupported property could fire, the infeasibility
code **MUST** take precedence.

- **Rationale**: The reported cause should be the one an operator can act on; the property
  mismatch is a consequence of the ceiling being hit.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

### 5.7 Trusted System Actors

#### Configured Trusted Actors

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-trusted-actors`

The plugin **MUST** support an explicitly configured set of trusted in-process system
actors. A request whose `(subject_type, subject_id)` matches an entry short-circuits to
Allow, skipping scope enforcement and subject-type classification.

- **Rationale**: Which actors exist, and under which identifiers, is a property of the
  deployment. An earlier revision compiled in two specific product identities, which meant
  every installation carried a bypass for actors it did not run.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

#### Empty by Default, Both Halves Matched

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-trusted-actor-pairing`

The set **MUST** be empty by default, both halves of a pair **MUST** match within the same
entry, and cross-pairing **MUST NOT** be trusted.

- **Rationale**: The subject identifier is the load-bearing half — it is minted in-process
  and never issued to a token holder, so a forged subject type alone cannot ride the bypass.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

#### Bypass Surface Is Logged

- [x] `p2` - **ID**: `cpt-cf-authz-plugin-fr-trusted-actor-count-logged`

The count of trusted actors **MUST** be logged at startup.

- **Rationale**: The widest bypass in the plugin should be visible in the log without
  reading configuration back out of the deployment.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

### 5.8 Plugin Discovery & Registration

#### Registration

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-registration`

The plugin **MUST** register a plugin instance in the types-registry at `init()` and publish
its client in `ClientHub`.

- **Rationale**: Both halves are how the gateway finds it; either one alone leaves a plugin
  that exists and is never called, or a hub entry pointing at nothing.
- **Actors**: `cpt-cf-authz-plugin-actor-gateway`, `cpt-cf-authz-plugin-actor-types-registry`

#### Vendor Has No Default

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-vendor-no-default`

The plugin's `vendor` setting **MUST** have no default.

- **Rationale**: An inherited default would make a gateway/plugin mismatch quiet — the
  plugin would register successfully and the failure would surface much later, as "no plugin
  instances found for vendor …" on the first authorization call.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

#### Dependencies Resolved at Startup

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-dependency-startup-failure`

Each required client dependency **MUST** be resolved at `init()`, and a missing one **MUST**
be a startup error rather than a degraded runtime mode.

- **Rationale**: A PDP that starts without its permission source cannot fail safe at request
  time in any way an operator would prefer to a failed startup.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

### 5.9 Audit

#### One Record per Completed Evaluation

- [x] `p2` - **ID**: `cpt-cf-authz-plugin-fr-audit-record`

Every completed evaluation — allow or business deny — **MUST** be auditable as one
structured record. Infrastructure errors **MUST NOT** produce audit records.

- **Rationale**: An audit trail is a record of decisions. An outage is not a decision; it
  belongs in logs and metrics.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

#### Structural Redaction

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-fr-audit-redaction`

Audit records **MUST NOT** carry bearer tokens or raw constraint predicates.

- **Rationale**: A constraint count and a fingerprint are sufficient to correlate a decision
  without reproducing what it authorized, and the exclusion is structural — the record type
  has no field for either.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

#### Disabled by Default

- [x] `p2` - **ID**: `cpt-cf-authz-plugin-fr-audit-disabled-by-default`

Audit emission **MUST** be disabled by default and enabled by configuration.

- **Rationale**: Emitting a record per authorization decision is a volume decision a
  deployment should take deliberately.
- **Actors**: `cpt-cf-authz-plugin-actor-platform-operator`

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs are defined at repository level — see the
> [architecture manifest](../../../../../../docs/ARCHITECTURE_MANIFEST.md) and
> [guidelines/](../../../../../../guidelines/). Only plugin-specific NFRs are recorded here.

### 6.1 Gear-Specific NFRs

#### Latency Attribution

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-nfr-latency-attribution`

Evaluation latency **MUST** be measured with the RBAC call and the hierarchy reads
instrumented separately from end-to-end decision time.

- **Threshold**: three distinct histograms — end-to-end, RBAC call, hierarchy reads
- **Rationale**: Evaluation latency is dominated by those two dependencies; without separate
  measurement a regression cannot be attributed to either.
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#313-metrics)

#### Hierarchy Cache Behaviour

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-nfr-hierarchy-cache`

Hierarchy reads **MUST** be cached with bounded size and TTL, and concurrent identical reads
**MUST** share one fetch.

- **Threshold**: bounded entry count and TTL, both configurable; single in-flight fetch per
  key
- **Rationale**: A hierarchy read is the second-most expensive step in an evaluation, and a
  thundering herd on one key would multiply it by the concurrency.
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#310-caching)

#### Fail-Closed Everywhere

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-nfr-fail-closed`

Every failure path **MUST** be fail-closed, and no configuration **MUST** be able to turn a
dependency failure into an allow.

- **Threshold**: zero configuration paths from failure to allow
- **Rationale**: A PDP that fails open converts any dependency outage into a
  platform-wide authorization bypass.
- **Verification Method**: automated tests plus inspection of the configuration surface
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#2-principles--constraints)

#### Machine-Readable Denials

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-nfr-machine-readable-denies`

Every deny **MUST** carry a machine-readable code in a single error namespace.

- **Threshold**: one namespace, `gts.cf.core.errors.err.v1~cf.authz.errors.<name>.v1`
- **Rationale**: Clients branch on codes; prose forces string parsing, which breaks on the
  first wording change.
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#312-deny-vocabulary)

#### Strict Configuration Loading

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-nfr-strict-config`

Unknown configuration keys **MUST** be rejected at startup.

- **Threshold**: every configuration section denies unknown fields
- **Rationale**: A typo in a security-relevant setting must not silently fall back to a
  default — `cach:` should name the field it could not accept.
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#311-configuration)

#### No Subject Identifiers in Logs

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-nfr-no-subject-in-logs`

Debug-level logging **MUST NOT** emit the subject identifier.

- **Threshold**: zero occurrences at any log level below audit
- **Rationale**: Debug logging is the easiest place for an identifier to leak into a sink
  with a different retention policy than the audit trail.
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#2-principles--constraints)

### 6.2 NFR Exclusions

- **Decision-cache freshness targets**: excluded — the plugin caches hierarchy and type
  lookups only. See [§13 Open Questions](#13-open-questions).
- **Enforcement verification**: excluded by construction. The plugin cannot observe a PEP's
  query, so it cannot carry a requirement about whether constraints were applied.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### Plugin Client Trait

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-interface-plugin-client`

- **Type**: Rust trait — the AuthZ Resolver SDK's plugin client, one method taking an
  evaluation request and returning an evaluation response
- **Stability**: stable
- **Description**: The only surface the gateway calls. Consumers do not depend on this
  crate: enforcement points use the SDK's PEP helpers, and the gateway routes to whichever
  plugin its vendor selects.
- **Breaking Change Policy**: the trait belongs to the AuthZ Resolver SDK; a change there is
  a major version bump for every plugin implementation

#### Deny Code Namespace

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-interface-deny-codes`

- **Type**: error-code vocabulary, `gts.cf.core.errors.err.v1~cf.authz.errors.<name>.v1`
- **Stability**: stable
- **Description**: One namespace so a client can branch on the code without parsing prose:

| Code | Meaning |
|------|---------|
| `scope_mismatch.v1` | Token scopes do not authorize the action |
| `insufficient_permissions.v1` | RBAC denied |
| `unknown_resource_type.v1` | Resource type is not registered (strict mode) |
| `unsupported_property.v1` | The PEP cannot evaluate a predicate the decision requires |
| `expansion_infeasible.v1` | The identifier expansion exceeds the configured ceiling |
| `constraints_unavailable.v1` | Constraints were required but could not be produced |

- **Breaking Change Policy**: codes are additive; removing or repurposing one is a breaking
  change for every client that branches on it

### 7.2 External Integration Contracts

#### Constraint Vocabulary

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-contract-constraints`

- **Direction**: provided by the plugin
- **Protocol/Format**: in-process typed predicates — tenant predicate, group predicate,
  push-down subtree predicate, and OR-combinations of them
- **Compatibility**: a new predicate class is only emitted to a PEP that declares the
  matching capability, so adding one is backward-compatible by construction

#### Capability Declaration

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-contract-capabilities`

- **Direction**: required from the PEP
- **Protocol/Format**: declared predicate classes (tenant hierarchy, group membership, group
  hierarchy) plus the set of properties the PEP can evaluate
- **Compatibility**: an undeclared capability degrades rather than fails; an undeclared
  *property* denies, because emitting it would produce an unconstrained read

## 8. Use Cases

#### Authorize a single-resource read

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-usecase-single-read`

**Actor**: `cpt-cf-authz-plugin-actor-pep`

**Preconditions**:
- The request is authenticated and carries token scopes

**Main Flow**:
1. The PEP asks whether a subject may read one resource
2. The plugin validates the request, then the types it names
3. The plugin enforces token scopes, then asks RBAC for the permission decision
4. The plugin returns Allow with the constraints that bound the read

**Postconditions**:
- The PEP applies the constraints even for a single resource — a decision without applied
  constraints is not an authorized read

**Alternative Flows**:
- **RBAC denies**: business deny with the insufficient-permissions code

#### Authorize a collection listing

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-usecase-collection-listing`

**Actor**: `cpt-cf-authz-plugin-actor-pep`

**Preconditions**:
- The PEP has declared which predicate classes and properties it can evaluate

**Main Flow**:
1. The PEP asks whether a subject may list a resource type
2. The plugin returns Allow plus a constraint set — a tenant subtree predicate, a group
   closure, or both OR-combined
3. The PEP pushes the predicates into its own query

**Postconditions**:
- Nothing outside the constraint set is returned, and the PEP never sees the roles that
  produced it

**Alternative Flows**:
- **A required property is not declared**: deny with the unsupported-property code

#### Refuse an infeasible expansion

- [x] `p1` - **ID**: `cpt-cf-authz-plugin-usecase-infeasible-expansion`

**Actor**: `cpt-cf-authz-plugin-actor-platform-operator`

**Preconditions**:
- A grant materializes into more identifiers than the configured ceiling

**Main Flow**:
1. The plugin denies with the infeasibility code
2. The operator raises the ceiling, or has the PEP declare the push-down capability

**Postconditions**:
- The remedy is visible in configuration or in the PEP's capabilities; nothing was silently
  truncated

## 9. Acceptance Criteria

- [x] A structurally invalid request is denied before any downstream call
- [x] An empty token-scope set denies
- [x] A namespaced scope satisfies its class; a longer unrelated scope with the same prefix
      does not
- [x] An unclassifiable subject type denies, and a group as direct subject denies
- [x] An RBAC deny surfaces as a business deny with the insufficient-permissions code; an
      RBAC outage surfaces as an infrastructure error
- [x] Every allowed decision carries constraints, and a group constraint always includes its
      owning tenant predicate
- [x] Barrier mode `respect` stops a subtree walk at a self-managed boundary; `ignore`
      traverses it
- [x] A non-Active granted root is excluded from an eager expansion; descendants are not
      status-clamped when the request carries no status filter
- [x] A declared tenant-hierarchy capability yields a push-down predicate; its absence yields
      an explicit identifier list
- [x] An expansion over the ceiling denies with the infeasibility code and is never truncated
- [x] Infeasibility outranks unsupported-property when both apply
- [x] A predicate whose property is not supported by the PEP denies rather than being emitted
- [x] An unknown resource type denies in strict mode, proceeds with a warning in warn mode,
      and is not consulted in off mode
- [x] Registry unavailability is not cached
- [x] A trusted system actor is honoured only when both halves of the configured pair match,
      and the default set is empty
- [x] A missing client dependency fails `init()`
- [x] Barrier and status semantics are applied during constraint generation, not by RBAC's
      scope inheritance
- [x] An audit record is emitted for allow and business deny, carries no bearer token and no
      raw predicates, and is off by default

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| RBAC gear | The permission decision itself | p1 |
| Tenant Resolver | Tenant ancestry, subtree expansion, barrier and status filtering | p1 |
| Resource Group gear | Group membership and closure | p1 |
| Types Registry | Type validity, and the plugin's own instance registration | p1 |
| AuthZ Resolver gateway | Selects and invokes the plugin | p1 |

## 11. Assumptions

- The security context reaching the plugin is already authenticated.
- Subject identifiers are stable and match what RBAC stores as principal identifiers.
- PEPs apply returned constraints faithfully; the plugin cannot verify this.
- Tenant and group hierarchies change infrequently relative to the cache TTL.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| A PEP ignores constraints and over-returns | Unconstrained read that looks authorized | Constraint application is a PEP contract; the SDK's helpers exist so it is not hand-rolled |
| A trusted system actor is configured too broadly | A bypass wider than the deployment intended | Empty by default, both halves matched, count logged at startup |
| Cache staleness after a hierarchy change | A decision made against a superseded hierarchy | Bounded TTL; event-driven invalidation is configurable and reserved for the Event Broker integration |
| Vendor mismatch between gateway and plugin | The plugin registers and is never selected | No default vendor, so the mismatch is a configuration error rather than an inherited one |
| Trusted actors combined with `strict` type validation | A trusted actor is denied before the bypass is consulted | Documented as a known limitation with two remedies — stay on `warn`, or add the skip to the validator ([DESIGN.md](./DESIGN.md#38-trusted-system-actors)) |

## 13. Open Questions

| Question | Status | Notes |
|----------|--------|-------|
| Should decision results themselves be cached, and under what invalidation? | Deferred | The cache exists for hierarchy and type reads only |
| Should event-driven cache invalidation become the default once the Event Broker ships? | Open | Configurable today, TTL-only by default |
| Should the plugin publish its constraint vocabulary as a GTS type for out-of-process PDPs? | Open | Today the vocabulary is an in-process type only |
| Should the type validator skip trusted actors so the two features compose under `strict`? | Open | The current behaviour denies rather than leaks, which is the safe direction but a confusing one to debug |

## 14. Traceability

Requirement to design section:

| Requirements | Design |
|--------------|--------|
| `cpt-cf-authz-plugin-fr-request-validation`, `cpt-cf-authz-plugin-fr-validation-before-io` | [DESIGN.md](./DESIGN.md#31-evaluation-pipeline) §3.1 |
| `cpt-cf-authz-plugin-fr-gts-type-validation`, `cpt-cf-authz-plugin-fr-validation-order`, `cpt-cf-authz-plugin-fr-type-cache-outage` | [DESIGN.md](./DESIGN.md#33-gts-type-validator) §3.3 |
| `cpt-cf-authz-plugin-fr-empty-scope-denies`, `cpt-cf-authz-plugin-fr-action-scope-mapping`, `cpt-cf-authz-plugin-fr-scope-match-exactness`, `cpt-cf-authz-plugin-fr-wildcard-scope`, `cpt-cf-authz-plugin-fr-scope-not-a-grant`, `cpt-cf-authz-plugin-fr-scope-class-derivation` | [DESIGN.md](./DESIGN.md#34-scope-enforcer) §3.4 |
| `cpt-cf-authz-plugin-fr-subject-type-classification`, `cpt-cf-authz-plugin-fr-groups-not-subjects`, `cpt-cf-authz-plugin-fr-delegate-to-rbac`, `cpt-cf-authz-plugin-fr-rbac-failure-distinct`, `cpt-cf-authz-plugin-fr-scope-provenance` | [DESIGN.md](./DESIGN.md#35-policy-evaluator) §3.5 |
| `cpt-cf-authz-plugin-fr-constraint-materialization`, `cpt-cf-authz-plugin-fr-group-tenant-pairing`, `cpt-cf-authz-plugin-fr-barrier-status`, `cpt-cf-authz-plugin-fr-supported-properties`, `cpt-cf-authz-plugin-fr-empty-materialization-denies` | [DESIGN.md](./DESIGN.md#36-materialization--constraint-generation) §3.6 |
| `cpt-cf-authz-plugin-fr-pushdown-predicate`, `cpt-cf-authz-plugin-fr-capability-degradation`, `cpt-cf-authz-plugin-fr-expansion-ceiling`, `cpt-cf-authz-plugin-fr-infeasibility-precedence` | [DESIGN.md](./DESIGN.md#37-capability-negotiation--degradation) §3.7 |
| `cpt-cf-authz-plugin-fr-trusted-actors`, `cpt-cf-authz-plugin-fr-trusted-actor-pairing`, `cpt-cf-authz-plugin-fr-trusted-actor-count-logged` | [DESIGN.md](./DESIGN.md#38-trusted-system-actors) §3.8 |
| `cpt-cf-authz-plugin-fr-registration`, `cpt-cf-authz-plugin-fr-vendor-no-default`, `cpt-cf-authz-plugin-fr-dependency-startup-failure` | [DESIGN.md](./DESIGN.md#32-packaging-registration--lifecycle) §3.2 |
| `cpt-cf-authz-plugin-fr-audit-record`, `cpt-cf-authz-plugin-fr-audit-redaction`, `cpt-cf-authz-plugin-fr-audit-disabled-by-default` | [DESIGN.md](./DESIGN.md#39-audit) §3.9 |
| `cpt-cf-authz-plugin-nfr-hierarchy-cache` | [DESIGN.md](./DESIGN.md#310-caching) §3.10 |
| `cpt-cf-authz-plugin-nfr-strict-config` | [DESIGN.md](./DESIGN.md#311-configuration) §3.11 |
| `cpt-cf-authz-plugin-nfr-machine-readable-denies` | [DESIGN.md](./DESIGN.md#312-deny-vocabulary) §3.12 |
| `cpt-cf-authz-plugin-nfr-latency-attribution` | [DESIGN.md](./DESIGN.md#313-metrics) §3.13 |
| `cpt-cf-authz-plugin-nfr-fail-closed`, `cpt-cf-authz-plugin-nfr-no-subject-in-logs` | [DESIGN.md](./DESIGN.md#2-principles--constraints) §2 |

Other artifacts:

- **Design**: [DESIGN.md](./DESIGN.md)
- **Plugin operations**: [README.md](../README.md)
- **Upstream role semantics**: [RBAC PRD](../../../../rbac/docs/PRD.md),
  [RBAC design](../../../../rbac/docs/DESIGN.md)
- **ADRs**: none recorded for this plugin
- **Features**: no feature specifications exist for this plugin yet
