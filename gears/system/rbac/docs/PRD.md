Created:  2026-08-20 by Constructor Fabric

# PRD — RBAC

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
  - [5.1 Role Model](#51-role-model)
  - [5.2 Role Assignment](#52-role-assignment)
  - [5.3 Permission Semantics](#53-permission-semantics)
  - [5.4 Scope Inheritance](#54-scope-inheritance)
  - [5.5 Multi-Role Evaluation](#55-multi-role-evaluation)
  - [5.6 Type Identification](#56-type-identification)
  - [5.7 Management API Semantics](#57-management-api-semantics)
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
> RG = Resource Group, GTS = Global Type System. Used throughout this document.

## 1. Overview

### 1.1 Purpose

The RBAC gear is the platform's source of truth for role-based access control on the
**management plane**. It owns role definitions, role assignments, and the scoped
permission semantics that a policy decision point consumes. It centralizes the platform
role model and role administration so that domain gears enforce one authorization model
rather than each inventing its own.

Its capabilities are: scoped role administration across global, tenant, and
resource-group boundaries; immutable built-in roles alongside tenant-owned custom roles
with tenant-safe delegation; additive permission semantics with role-local exclusions
through `not_permissions`; unconditional downward inheritance across the supported scope
model; and an in-process permission-query contract that the AuthZ Resolver Plugin calls
on every management-plane authorization decision.

Authentication and identity stay outside this gear. Deciding a request and compiling the
constraints a PEP applies stay outside it too — those belong to the PDP, whose
requirements are in the [AuthZ Resolver Plugin PRD](../../authz-resolver/plugins/authz-resolver-plugin/docs/PRD.md).

### 1.2 Background / Problem Statement

Without a shared role model, every gear invents its own notion of who may do what, and an
operator has no single place to grant or revoke access. Two failures follow. Delegation
becomes unsafe: a tenant administrator granting access has no mechanism that stops the
grant exceeding their own scope. And the baseline is untrustworthy: a role set that any
administrator can edit gives a security officer nothing to reason from.

This gear exists so the model is defined once. Role definitions carry a
`{ operation, target_type }` permission vocabulary, assignments bind a principal to a role
at a scope, and inheritance across the scope tree is a property of the model rather than a
per-consumer interpretation.

Authorization is a pipeline, and this gear owns exactly one stage of it:
`effective_access = min(token_scopes, user_permissions)`. Token-scope intersection, tenant
barrier semantics (self-managed tenant visibility), constraint compilation, and PEP
enforcement rules are the PDP's; they are specified in the
[AuthZ Resolver Plugin PRD](../../authz-resolver/plugins/authz-resolver-plugin/docs/PRD.md)
and deliberately not re-defined here.

### 1.3 Goals (Business Outcomes)

- A platform administrator defines reusable role definitions with scoped permissions, so
  the platform's authorization model is consistent and governable across gears
- A tenant administrator delegates access inside their own tenant subtree without any path
  to exceed their own scope
- A security officer gets predictable permission subtraction (`not_permissions` are
  role-local) and a built-in role set that cannot be tampered with
- One permission query serves every management-plane authorization decision, inside a
  latency budget the request path can absorb

### 1.4 Glossary

| Term | Definition |
|------|------------|
| PDP | Policy Decision Point that evaluates whether access should be allowed |
| PEP | Policy Enforcement Point in a domain gear that applies the authorization outcome |
| Subject | User, group, or service principal requesting access |
| Scope | The boundary where a role applies: global (`/`), tenant, or resource-group |
| Role Definition | Reusable permission set with `permissions`, `not_permissions`, and `assignable_scopes` |
| Role Assignment | Binding between a principal, a role definition, and a scope |
| Built-in Role | Platform-provided role that administrators cannot modify or delete |
| Custom Role | Tenant-owned role for deployment-specific delegation needs |
| GTS Type Family | Wildcard over a GTS type prefix, used to group resource types for permission matching |
| Permission Catalog | The platform-wide inventory of `{ action, resource_type }` pairs declared by registered gears |

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level by the steering committee.
> This section documents the actors that interact with the gear.

### 2.1 Human Actors

#### Platform Administrator

**ID**: `cpt-cf-rbac-actor-platform-admin`

- **Role**: Holds `Owner` at scope `/`. Administers built-in role visibility, creates
  tenant-owned custom roles on behalf of a tenant, and manages platform-wide assignments.
- **Needs**: A governable, reusable role model and a bootstrap path that exists before any
  authenticated call is possible.

#### Tenant Administrator

**ID**: `cpt-cf-rbac-actor-tenant-admin`

- **Role**: A persona rather than a built-in role — realized by a tenant-scoped `Owner`
  assignment or by a delegated custom role carrying role-definition management
  permissions. Creates custom roles and manages assignments inside one tenant subtree.
- **Needs**: Delegation that cannot exceed the tenant's own boundary.

#### Security Officer

**ID**: `cpt-cf-rbac-actor-security-officer`

- **Role**: Reviews the delegation model and the baseline role set.
- **Needs**: Immutable built-in roles, role-local exclusion semantics, and enforced
  assignable-scope boundaries.

#### Platform Operator

**ID**: `cpt-cf-rbac-actor-platform-operator`

- **Role**: Configures the deployment — which resource-type families the built-in roles
  grant, which principals hold a role from first boot, and whether the integration roles
  are seeded.
- **Needs**: Configuration that fails loudly at startup rather than producing a role that
  assigns cleanly and authorizes nothing.

### 2.2 System Actors

#### AuthZ Resolver Plugin (PDP)

**ID**: `cpt-cf-rbac-actor-pdp`

- **Role**: The only in-process consumer of the permission-query contract in v1. Resolves
  `RbacServiceClientV1` from `ClientHub`, asks for a subject's roles or a single permission
  decision, and turns the answer into a decision plus constraints.

#### Domain Gears (PEPs)

**ID**: `cpt-cf-rbac-actor-pep`

- **Role**: Call the PDP and enforce the returned outcome at their own boundary. They never
  read RBAC data directly.

#### Tenant Resolver

**ID**: `cpt-cf-rbac-actor-tenant-resolver`

- **Role**: Authoritative read contract for tenant existence and ancestry, used to validate
  scope paths and to walk ancestor scopes. Account Management remains the tenant source of
  truth behind it.

#### Resource Group Gear

**ID**: `cpt-cf-rbac-actor-resource-group`

- **Role**: Owns user groups and resource-group hierarchy. Supplies group existence, tenant
  ownership, and membership resolution for `Group` principals.

#### Types Registry

**ID**: `cpt-cf-rbac-actor-types-registry`

- **Role**: Holds the RBAC entity schemas registered at startup, validates the
  `target_type` values written into custom roles, and stores the permission-catalog
  instances the gear serves.

## 3. Operational Concept & Environment

Runtime, OS, lifecycle policy, and integration patterns are defined once at the repository
level — see the [architecture manifest](../../../../docs/ARCHITECTURE_MANIFEST.md) and the
foundational [guidelines/](../../../../guidelines/). This gear has no parent gear PRD; the
authorization-wide context it sits in is
[docs/arch/authorization/DESIGN.md](../../../../docs/arch/authorization/DESIGN.md). Only
gear-specific deviations are recorded below.

### 3.1 Gear-Specific Environment Constraints

- **PostgreSQL is the production backend.** SQLite is supported for tests, development, and
  embedded demos, but the migrations drop the GIN / `pg_trgm` / `text_pattern_ops` indexes
  there, so `LIKE` filtering degrades to full scans.
- **The permission-query contract is in-process only.** It is resolved through `ClientHub`
  and has no network surface; there is no REST endpoint for permission evaluation.
- **Runtime event publication is not available.** RBAC change events are reserved as GTS
  type identifiers and schema placeholders; publication waits on the platform Event Broker
  integration and is not a launch dependency.

## 4. Scope

### 4.1 In Scope

- Role-definition administration — built-in roles immutable, custom roles tenant-owned and
  mutable
- Role-assignment administration with scope validation and duplicate rejection
- Built-in role seeding at startup, with the resource-type families the roles grant named
  by deployment configuration
- Startup grants for principals that must hold a role before any authenticated call is
  possible
- Additive permission semantics: union across roles, `not_permissions` subtracting within
  their own role only
- Type-family and operation wildcard matching on permission rules
- Unconditional downward scope inheritance across global, tenant-subtree, and
  resource-group-subtree scopes
- The in-process permission-query contract consumed by the PDP
- Publication of the platform permission catalog over the management API
- GTS-compliant entity schemas, registered at startup

### 4.2 Out of Scope

- **Data-plane authorization** — v1 covers management-plane access only
- **Authentication, SSO, and identity lifecycle** — see
  [docs/arch/authorization/DESIGN.md](../../../../docs/arch/authorization/DESIGN.md)
- **Tenant and resource-group source of truth** — owned by
  [Account Management](../../account-management/docs/PRD.md) and the
  [Resource Group gear](../../resource-group/docs/PRD.md)
- **Quota, budget, and non-RBAC policy domains** — see
  [Quota Enforcement](../../quota-enforcement/docs/PRD.md)
- **PDP evaluation, constraint generation, PEP enforcement, hierarchy projections** —
  see the [AuthZ Resolver Plugin PRD](../../authz-resolver/plugins/authz-resolver-plugin/docs/PRD.md)
- **Role-to-role inheritance** — v1 supports assignment-scope inheritance only; parent-role
  hierarchies and derived-role chains are not part of the baseline model
- **Conditional (ABAC-style) permissions** — deferred until the model is extended beyond
  scoped RBAC
- **Cross-tenant role sharing** — deferred beyond the baseline release
- **Authorization-decision caching** — deferred until invalidation and freshness guarantees
  are defined
- **RBAC mutation audit trail** — deferred until the platform Event Broker and audit
  infrastructure are available; v1 persists current state only

## 5. Functional Requirements

> **Testing strategy**: All requirements are verified via automated tests (unit,
> integration, API, e2e) targeting 90%+ coverage unless otherwise specified. Verification
> method is documented only where a non-test approach applies.

### 5.1 Role Model

#### Role Definition Structure

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-role-definition-structure`

The system **MUST** store a role definition as `permissions`, `not_permissions`, and
`assignable_scopes`, where every permission rule is a pair of an `operation` and a
`target_type`.

- **Rationale**: The `{ operation, target_type }` vocabulary is what makes permissions
  composable across gears without a synthetic action-string registry.
- **Actors**: `cpt-cf-rbac-actor-platform-admin`, `cpt-cf-rbac-actor-tenant-admin`

#### Built-in Role Set

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-builtin-roles`

The system **MUST** provide four core built-in roles — `Owner`, `Contributor`, `Reader`,
`User Access Administrator` — on every deployment, and **MUST** reject any attempt to
modify or delete a built-in role. Two further built-in roles, `Credstore Secret Operator`
and `Usage Emitter`, **MUST** be seeded only when the deployment opts into them.

- **Rationale**: The core four are the common administration paths. The integration pair
  targets other gears' resource types, so a deployment without those gears would otherwise
  inherit roles that authorize types nobody registered.
- **Actors**: `cpt-cf-rbac-actor-platform-admin`, `cpt-cf-rbac-actor-security-officer`

#### Built-in Role Targets Named by the Deployment

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-builtin-role-targets`

The system **MUST** take the resource-type families that `Owner`, `Contributor`, and
`Reader` grant from deployment configuration rather than from compiled-in values. A family
list that names none of RBAC's own types **MUST** be reported at startup, and an empty
family list **MUST** be refused.

- **Rationale**: Role ids and names are a cross-deployment contract — consumers resolve
  `Owner` by id and grants resolve roles by name — so they are deliberately fixed. What a
  role *grants* is not. A compiled-in vendor wildcard leaves three of the four core roles
  authorizing nothing on any platform but one. The line between the two is what makes the
  role model reusable.
- **Actors**: `cpt-cf-rbac-actor-platform-operator`

> A list naming none of RBAC's own types yields an `Owner` who cannot administer role
> assignments, including the ones that would fix it — hence the startup warning. An empty
> list would seed a role that exists, assigns cleanly, and authorizes nothing — hence the
> refusal. `User Access Administrator` targets RBAC's own types and is not configurable.

#### Startup Grants

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-startup-grants`

The system **MUST** grant each configured principal its named built-in role at scope `/`,
idempotently on every boot. The `principal_type` written **MUST** be determined by which
configured list the entry appears in, never by an operator-supplied field. A grant naming a
role the deployment does not seed **MUST** abort startup.

- **Rationale**: The management API cannot authorize the first administrator, and an
  out-of-process system actor — a metering worker, a secret writer — has no in-process
  trust path to fall back on. Both need a grant that exists before any authenticated call
  is possible. Everything beyond that first grant belongs on the API.
- **Actors**: `cpt-cf-rbac-actor-platform-operator`

> Deriving `principal_type` from the list rather than a field is deliberate: the type must
> match what the caller's token classifies as, and a mistyped value would produce a valid
> configuration whose grant is never found — denying with no diagnostic at any layer.
> Aborting on an unseeded role name avoids writing an assignment that points at nothing.

#### Custom Role Creation

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-custom-roles`

The system **MUST** allow creation of a tenant-owned custom role whose every entry in
`assignable_scopes` is at or below the owner tenant's subtree. The role name **MUST** be
unique within the owner tenant and **MUST NOT** collide with a built-in role name
(case-insensitive).

- **Rationale**: Delegation is only safe if a custom role cannot reach outside the tenant
  that owns it; name collision with a built-in would make two roles indistinguishable in
  the same result set.
- **Actors**: `cpt-cf-rbac-actor-tenant-admin`, `cpt-cf-rbac-actor-platform-admin`

> A platform administrator may create a tenant-owned custom role on behalf of a tenant, but
> the request **MUST** identify the owner tenant explicitly and the result stays scoped to
> that tenant subtree. v1 defines no global custom roles.

### 5.2 Role Assignment

#### Role Assignment Creation

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-role-assignment`

The system **MUST** create a role assignment from `principal_id`, `principal_type`,
`role_definition_id`, and `scope`, after which the principal holds the role's permissions
at that scope and below.

- **Rationale**: The principal + role + scope triple is the whole grant model; inheritance
  downward from the assignment point is what keeps the number of assignments bounded.
- **Actors**: `cpt-cf-rbac-actor-platform-admin`, `cpt-cf-rbac-actor-tenant-admin`

#### Assignable-Scope Enforcement

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-assignable-scopes`

The system **MUST** reject an assignment whose scope is not a listed `assignable_scope` of
the role or a descendant of one.

- **Rationale**: `assignable_scopes` is the boundary that makes a delegated role safe to
  hand out; without enforcement at every write it is documentation rather than a control.
- **Actors**: `cpt-cf-rbac-actor-security-officer`

#### Roles of a Principal

- [ ] `p2` - **ID**: `cpt-cf-rbac-fr-principal-role-visibility`

The system **MUST** let an administrator list the roles currently assigned to one user or
one user group together with their scopes, assign a further role to that principal with the
principal already chosen, and revoke from that list with the same effect as revoking from
the assignments list.

- **Rationale**: An administrator answering "what can this person do?" should not have to
  reconstruct it from a global assignments list.
- **Actors**: `cpt-cf-rbac-actor-platform-admin`, `cpt-cf-rbac-actor-tenant-admin`
- **Acceptance Evidence**: `GET /rbac/v1/role-assignments?principal_id=…` serves the data;
  the presentation is built outside this repository (see
  [§13 Open Questions](#13-open-questions)).

> **Ownership split.** This PRD owns the role-bearing part of a principal's view: which
> roles are held, at which scopes, and the assign/revoke actions. It does not own the
> principal's identity, status, MFA, last activity, or group membership — those belong to
> [Account Management](../../account-management/docs/PRD.md). Keeping the two concerns in
> separate documents is what stops them drifting into competing specifications of the same
> fields.

### 5.3 Permission Semantics

#### Type-Family Wildcard Matching

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-type-family-wildcard`

The system **MUST** match a permission rule whose `target_type` is a GTS family wildcard
against any concrete request `resource.type` inside that family — a rule on
`gts.vendor.resources.compute.*` grants a request on
`gts.vendor.resources.compute.vm.v1~`.

- **Rationale**: Roles are written against domains, not against every type a domain will
  ever publish.
- **Actors**: `cpt-cf-rbac-actor-pdp`

#### Operation Wildcard Matching

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-operation-wildcard`

The system **MUST** treat `operation: "*"` as matching any requested action, both over a
family target and over a single narrowed target.

- **Rationale**: "All operations" is a first-class administrative choice, not an artifact of
  the schema's permissiveness, so its semantics need an owner here.
- **Actors**: `cpt-cf-rbac-actor-platform-admin`, `cpt-cf-rbac-actor-tenant-admin`

> **Grow-forward consequence, stated deliberately.** A wildcard rule grants resource types
> that do not exist yet: a type added to a matching family by a future release falls under
> an existing `*` rule with no administrator action. This follows directly from family
> matching rather than being new behaviour, but it is the kind of property an administrator
> granting access is entitled to be told about. Team confirmation is still outstanding —
> see [§13 Open Questions](#13-open-questions).
>
> **Ceiling.** There is no platform-wide wildcard: the type vocabulary requires at least one
> segment before the wildcard, so `gts.*` is not a valid `target_type`. The broadest
> expressible grant is per vendor (`gts.cf.*`, or the equivalent over another vendor prefix).

#### Direct Permission-Rule Entry

- [ ] `p2` - **ID**: `cpt-cf-rbac-fr-raw-permission-rule`

The system **MUST** accept a permission rule entered directly as an `operation` plus a
`target_type` when the guided permission picker cannot express it, **MUST** validate the
value against the contract vocabulary before submission so an invalid rule is refused with
a reason, and **MUST** treat a directly entered rule as indistinguishable from a picked one.

- **Rationale**: The guided picker is bounded by the published catalog; a role legitimately
  needs rules the catalog does not yet list — a newly registered type, a namespace-level
  grant. Without a direct-entry path the administrator's only option is to wait for the
  catalog. The validation half is equally normative: a client must not offer or accept a
  value the service will reject, in particular a bare platform-wide wildcard.
- **Actors**: `cpt-cf-rbac-actor-tenant-admin`
- **Acceptance Evidence**: the service-side half ships — the catalog is served by
  `GET /rbac/v1/permissions` and `target_type` is validated against the types-registry on
  write. The authoring surface is built outside this repository.

#### Role-Local `not_permissions` Exclusion

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-not-permissions`

The system **MUST** subtract a matching `not_permissions` rule from the grants of the role
that declares it, denying a request that its own `permissions` would otherwise allow.

- **Rationale**: Carving an exception out of a broad grant is the common administrative
  need; making the subtraction global would turn one role's exception into a
  platform-wide deny nobody can locate.
- **Actors**: `cpt-cf-rbac-actor-security-officer`

### 5.4 Scope Inheritance

#### Tenant-Subtree Inheritance

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-scope-inheritance`

The system **MUST** apply a grant assigned at a tenant to every descendant tenant in the
hierarchy.

- **Rationale**: Without downward inheritance, a parent-tenant administrator would need one
  assignment per descendant, and every new tenant would silently lose inherited access.
- **Actors**: `cpt-cf-rbac-actor-platform-operator`, `cpt-cf-rbac-actor-pdp`

#### Resource-Group Subtree Grant

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-rg-subtree-grant`

The system **MUST** apply a grant assigned at `/tenants/{t}/resourceGroups/{rg}` within
that resource group's subtree, including descendant groups under `rg`.

- **Rationale**: Resource groups nest, and an administrator granting access to a branch
  means the branch.
- **Actors**: `cpt-cf-rbac-actor-pdp`

#### Scope Taxonomy

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-scope-taxonomy`

The system **MUST** produce exactly the scope variants below for the PDP to consume, and
**MUST NOT** produce a reserved variant in v1.

| Scope variant | v1 status | Semantics |
|---------------|-----------|-----------|
| Global | Active | Grant applies platform-wide |
| Tenant subtree | Active | Grant applies at the assigned tenant and all descendants |
| Resource-group subtree | Active | Grant applies at the assigned group and all descendant groups in the same tenant |
| Tenant direct (`TenantDirect`) | Reserved | Grant applies at exactly one tenant without subtree inheritance |
| Resource-group direct membership (`ExplicitGroups`) | Reserved | Grant applies only to resources directly in the assigned group, no subtree expansion |

- **Rationale**: The PDP branches on the variant to build constraints. Naming the reserved
  variants keeps the enum open for extension while making it unambiguous that v1 never
  emits them, so a consumer can fail closed on anything else.
- **Actors**: `cpt-cf-rbac-actor-pdp`

> **Inheritance model.** Every active variant uses unconditional downward inheritance:
> there is no per-assignment opt-out, no `excluded_scopes`, and no tenant-direct variant in
> v1. Rationale and the future extension path are recorded in
> [DESIGN.md](./DESIGN.md#4-additional-context) under resolved design decisions.
>
> **Barrier note.** Tenant barrier enforcement — excluding self-managed subtrees from
> visibility — is applied by the PDP during constraint generation, not by this scope model:
> see the barrier-and-status requirement in the
> [AuthZ Resolver Plugin PRD](../../authz-resolver/plugins/authz-resolver-plugin/docs/PRD.md#55-constraint-generation).

#### Reserved Direct-Membership Scope

- [ ] `p3` - **ID**: `cpt-cf-rbac-fr-reserved-direct-membership`

The system **MUST NOT** produce the direct-membership scope variant (`ExplicitGroups`) in
v1; a future extension **MUST** apply it without inheritance into child groups.

- **Rationale**: Fine-grained scoping mechanisms will need membership without subtree
  expansion, and reserving the variant now avoids a breaking enum change later.
- **Actors**: `cpt-cf-rbac-actor-pdp`

### 5.5 Multi-Role Evaluation

#### Additive Union Across Roles

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-additive-union`

The system **MUST** treat a subject's effective permissions as the union of all role
assignments applicable to the request.

- **Rationale**: Roles are composed, not ranked; a second role must be able to add an
  operation the first does not carry.
- **Actors**: `cpt-cf-rbac-actor-pdp`

#### Exclusions Are Role-Scoped

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-role-scoped-exclusion`

The system **MUST** confine a `not_permissions` match to the role that declares it: another
role's grant for the same request **MUST** still allow it, and v1 **MUST NOT** define a
global explicit-deny stage.

- **Rationale**: A global deny would make the outcome depend on which roles a subject
  happens to hold elsewhere, which is exactly the unpredictability the additive model
  exists to avoid.
- **Actors**: `cpt-cf-rbac-actor-security-officer`, `cpt-cf-rbac-actor-pdp`

#### Deterministic Evaluation

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-deterministic-evaluation`

The system **MUST** produce the same outcome for a given
`{ subject, operation, resource.type, tenant context }` regardless of role ordering: the
request is allowed if at least one role grants it after that role's own `not_permissions`
are subtracted, and denied otherwise.

- **Rationale**: An authorization outcome that depends on row order is untestable and
  unauditable.
- **Actors**: `cpt-cf-rbac-actor-pdp`

> The step-by-step evaluation algorithm — assignment resolution, per-role matching,
> `not_permissions` application, surviving-role union — is owned by
> [DESIGN.md](./DESIGN.md#32-component-model).

### 5.6 Type Identification

#### Entity Schema Registration

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-entity-schemas`

The system **MUST** publish GTS-compliant schemas for its entities — `role_definition` and
`role_assignment` — with `$id` values in GTS form, register them at gear initialization, and
evolve them under GTS minor-version compatibility rules.

- **Rationale**: A shared type vocabulary is what lets other gears validate and interpret
  RBAC data without hand-written adapters.
- **Actors**: `cpt-cf-rbac-actor-types-registry`, `cpt-cf-rbac-actor-platform-operator`

#### Reserved Event Type Identifiers

- [ ] `p3` - **ID**: `cpt-cf-rbac-fr-reserved-event-types`

The system **MUST** reserve GTS type identifiers and schema placeholders for its five
mutation events, and **MUST NOT** publish them until the platform Event Broker contract is
available.

- **Rationale**: Reserving the identifiers now means a later integration lands the transport
  without renaming event types or reshaping payloads.
- **Actors**: `cpt-cf-rbac-actor-platform-operator`
- **Verification Method**: inspection — the placeholder schemas are pinned on disk; no
  runtime behaviour to test in v1.

### 5.7 Management API Semantics

> Protocol details, payload shapes, and the error-body format belong to
> [DESIGN.md](./DESIGN.md#33-api-contracts). This section defines the required outcome
> semantics only.

#### Unauthenticated Requests

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-unauthenticated`

The system **MUST** reject a request without valid authentication as unauthenticated, and
**MUST NOT** invoke any permission evaluation for it.

- **Rationale**: Authentication runs before authorization; evaluating an unauthenticated
  request would spend the hot path on a decision that cannot be honoured.
- **Actors**: `cpt-cf-rbac-actor-pep`

#### Insufficient Permission

- [x] `p1` - **ID**: `cpt-cf-rbac-fr-insufficient-permission`

The system **MUST** reject an authenticated management-API call made without the required
permission, returning a standardized, client-readable error defined by the design contract.

- **Rationale**: Clients branch on machine-readable errors; prose-only rejections force
  string parsing.
- **Actors**: `cpt-cf-rbac-actor-tenant-admin`

#### Human-Readable Names on Reads

- [x] `p2` - **ID**: `cpt-cf-rbac-fr-read-display-names`

A role-assignment read **MUST** carry the display name of the principal, of the row's
author, and of the granted role when they can be resolved, and **MUST** serve the row
without them when they cannot.

- **Rationale**: Every consumer otherwise resolves the same three identifiers itself or
  renders raw UUIDs. Resolution is decoration, so it must never be the reason a read fails,
  returns fewer rows, or breaks a cursor — a name that cannot be produced is an absent
  name, not an error.
- **Actors**: `cpt-cf-rbac-actor-tenant-admin`, `cpt-cf-rbac-actor-platform-admin`

#### Bounded Cost for Name Resolution

- [x] `p2` - **ID**: `cpt-cf-rbac-fr-name-resolution-bounded`

Name resolution **MUST** be bounded per tenant and per request, **MUST** be batched across
a page rather than issued per row, and **MUST** degrade to unnamed rows when a bound is
reached.

- **Rationale**: Naming a user is not a point read — the upstream serves a user listing out
  of a tenant's group membership and re-drains it per call. Unbounded resolution would make
  the cost of a page a function of its size and of how many tenants its rows happen to span,
  which is chosen by whoever wrote the assignments rather than by the operator.
- **Actors**: `cpt-cf-rbac-actor-platform-operator`

#### Role Usage Counts

- [x] `p2` - **ID**: `cpt-cf-rbac-fr-role-assignment-counts`

A role-definition read **MUST** report how many assignments of that role the caller can
see, and the catalog **MUST** expose built-in / custom / total counts under the same
visibility as the list endpoint. Both **MUST** report "unknown" rather than a number when
no honest count exists for the caller.

- **Rationale**: "Can I delete this role?" otherwise means listing assignments and counting
  by hand. Reporting `0` to a caller who can read no assignments would answer a different
  question than the one asked — and acting on it means deleting a role that is in use.
- **Actors**: `cpt-cf-rbac-actor-tenant-admin`, `cpt-cf-rbac-actor-platform-admin`

#### Permission Catalog Publication

- [x] `p2` - **ID**: `cpt-cf-rbac-fr-permission-catalog`

The system **MUST** publish the platform permission catalog — every `{ action,
resource_type }` pair declared by a registered gear — over the management API to any
authenticated caller, with filtering on action and resource-type prefix and stable
cursor pagination.

- **Rationale**: An administrator composing a role needs to browse what exists. The catalog
  is platform metadata, so gating it behind an RBAC `read` permission would create a
  recursive bootstrap in which the catalog must grant read on itself.
- **Actors**: `cpt-cf-rbac-actor-tenant-admin`, `cpt-cf-rbac-actor-platform-admin`

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs are defined at repository level — see the
> [architecture manifest](../../../../docs/ARCHITECTURE_MANIFEST.md) and
> [guidelines/](../../../../guidelines/). Only gear-specific NFRs are recorded here.
>
> **Testing strategy**: NFRs are verified via automated benchmarks, security scans, and
> monitoring unless otherwise specified.

### 6.1 Gear-Specific NFRs

#### In-Process Permission Query Latency

- [x] `p1` - **ID**: `cpt-cf-rbac-nfr-permission-query-latency`

The in-process permission query **MUST** complete within p95 ≤ 5 ms and p99 ≤ 10 ms.

- **Threshold**: p95 ≤ 5 ms, p99 ≤ 10 ms, sustained while the instance serves the
  concurrency floor below
- **Rationale**: Every authorization decision in the platform includes one permission
  query; higher latency cascades into REST, CLI, and portal response times
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#12-architecture-drivers) § NFR
  Allocation

#### Management API Latency

- [x] `p1` - **ID**: `cpt-cf-rbac-nfr-rest-latency`

The REST management API **MUST** respond within p95 ≤ 50 ms and p99 ≤ 100 ms.

- **Threshold**: p95 ≤ 50 ms, p99 ≤ 100 ms
- **Rationale**: Role administration must feel interactive for operators and tenant admins
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#12-architecture-drivers) § NFR
  Allocation

#### Availability

- [x] `p1` - **ID**: `cpt-cf-rbac-nfr-availability`

Service availability **MUST** be at least 99.95 % over a rolling 30-day window.

- **Threshold**: ≥ 99.95 % over 30 days
- **Rationale**: Authorization outages stop every write path in the platform
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#12-architecture-drivers) § NFR
  Allocation

#### Concurrency Floor

- [x] `p1` - **ID**: `cpt-cf-rbac-nfr-concurrency`

The gear **MUST** sustain at least 5,000 concurrent in-process permission queries per
instance, and at least 500 in-flight REST requests per instance, without violating the
latency targets above.

- **Threshold**: ≥ 5,000 concurrent in-process queries; ≥ 500 in-flight REST requests
- **Rationale**: Platform-wide authorization fan-out demands this floor even in
  single-instance deployments
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#12-architecture-drivers) § NFR
  Allocation

#### Delegation Invariants

- [x] `p1` - **ID**: `cpt-cf-rbac-nfr-delegation-invariants`

Built-in roles **MUST** be immutable and assignable-scope boundaries **MUST** be enforced
at every write.

- **Threshold**: zero tolerance — a single violated write breaks the delegation model
  platform-wide
- **Rationale**: The delegation model's integrity rests entirely on these two invariants
- **Verification Method**: automated tests plus inspection of the seeder's post-upsert
  assertions
- **Architecture Allocation**: See [DESIGN.md](./DESIGN.md#310-security-architecture)

### 6.2 NFR Exclusions

- **Mutation audit trail**: excluded from v1. RBAC persists current state only; a dedicated
  audit trail waits on the platform Event Broker and audit infrastructure. Startup-written
  rows remain distinguishable through `created_by` (`system`, `system-bootstrap`).
- **Authorization-decision audit**: excluded by ownership, not by deferral. The decision
  audit point sits with the PDP, which has the full request context; the gear's permission
  query is an internal data lookup.
- **Authorization-decision caching**: excluded from v1 pending invalidation and freshness
  guarantees. Hierarchy caching lives in the PDP.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### In-Process Permission Query Client

- [x] `p1` - **ID**: `cpt-cf-rbac-interface-sdk-client`

- **Type**: Rust trait (`RbacServiceClientV1`) in the transport-free `rbac-sdk` crate,
  resolved from `ClientHub`
- **Stability**: stable
- **Description**: Returns the roles a subject holds in a tenant context, and evaluates a
  single `{ operation, resource_type }` permission check. Carries the models, the error
  enum, and nothing else — no HTTP framework, ORM, or migrations.
- **Breaking Change Policy**: major version bump; the scope-type enum is open so new
  variants are not breaking for consumers that fail closed on unknown values

#### REST Management API

- [x] `p1` - **ID**: `cpt-cf-rbac-interface-rest-api`

- **Type**: REST/OpenAPI under `/rbac/v1`
- **Stability**: stable
- **Description**: Role-definition CRUD, role-assignment create/read/delete, the catalog
  counts summary, and the permission catalog. It is the administrative surface, not the path
  a PDP takes. Reads carry decoration — display names and assignment counts — which is
  nullable by contract and never affects a status code, a row set, or a cursor.
- **Breaking Change Policy**: new major path prefix (`/rbac/v2`) for any incompatible change

#### Entity Schemas

- [x] `p1` - **ID**: `cpt-cf-rbac-interface-entity-schemas`

- **Type**: GTS JSON Schemas registered in the types-registry at startup
- **Stability**: stable
- **Description**: `role_definition` and `role_assignment` schemas in GTS `$id` form, plus
  five reserved event-schema placeholders
- **Breaking Change Policy**: GTS minor-version compatibility rules; an incompatible change
  requires a new major type version

### 7.2 External Integration Contracts

#### Permission Query Contract

- [x] `p1` - **ID**: `cpt-cf-rbac-contract-permission-query`

- **Direction**: provided by the gear
- **Protocol/Format**: in-process Rust trait through `ClientHub`; no network boundary
- **Compatibility**: the consumer resolves subject identity and tenant context and the gear
  trusts those arguments, so every new consumer must document how it derives them

#### Scope Provider Contracts

- [x] `p1` - **ID**: `cpt-cf-rbac-contract-scope-providers`

- **Direction**: required from other gears
- **Protocol/Format**: in-process read contracts for tenant ancestry and resource-group
  hierarchy/membership
- **Compatibility**: a missing provider at startup is an initialization error, not a
  degraded mode

## 8. Use Cases

#### Delegate access inside a tenant

- [x] `p1` - **ID**: `cpt-cf-rbac-usecase-delegate-in-tenant`

**Actor**: `cpt-cf-rbac-actor-tenant-admin`

**Preconditions**:
- The actor holds a role carrying role-definition and role-assignment management
  permissions within the tenant

**Main Flow**:
1. The actor browses the permission catalog for the operations and resource types they need
2. The actor creates a custom role whose `assignable_scopes` lie inside the tenant subtree
3. The actor assigns the role to a user or user group at a tenant or resource-group scope
4. The subject's next request is authorized through the new grant

**Postconditions**:
- A tenant-owned custom role exists, and an assignment binds it to the principal at a scope
  inside the tenant subtree

**Alternative Flows**:
- **Scope outside the tenant subtree**: the create or assign call is rejected as invalid
- **Name collides with a built-in role**: the create call is rejected as a conflict

#### Authorize a management-plane request

- [x] `p1` - **ID**: `cpt-cf-rbac-usecase-authorize-request`

**Actor**: `cpt-cf-rbac-actor-pdp`

**Preconditions**:
- The request is authenticated and the PDP has resolved the subject and tenant context

**Main Flow**:
1. The PDP asks for the subject's roles in the tenant context, or for a single permission
   decision
2. The gear resolves the applicable assignments across ancestor scopes and the context
   tenant's resource groups
3. The gear matches the request against each role's `permissions`, subtracting that role's
   `not_permissions`
4. The gear returns the surviving grants and the aggregated scope type, or a typed denial

**Postconditions**:
- The PDP holds enough information to decide the request and to compile constraints

**Alternative Flows**:
- **No applicable assignment**: the gear returns a denial reason of no matching permission

#### Bootstrap a fresh deployment

- [x] `p1` - **ID**: `cpt-cf-rbac-usecase-bootstrap`

**Actor**: `cpt-cf-rbac-actor-platform-operator`

**Preconditions**:
- The deployment configures the built-in role targets and the principals that must hold a
  role from first boot

**Main Flow**:
1. The gear validates the configuration and aborts on an empty target list or an unseeded
   role name
2. The gear upserts the built-in roles idempotently
3. The gear writes the configured grants at scope `/`, idempotently
4. The gear publishes its client and mounts its REST routes

**Postconditions**:
- Somebody can sign in and administer the platform, and configured system actors have real
  grants rather than a PEP bypass

**Alternative Flows**:
- **No platform administrator configured**: the step is skipped with a warning; no phantom
  default is invented

## 9. Acceptance Criteria

- [x] A role definition stores `permissions`, `not_permissions`, and `assignable_scopes`,
      each rule carrying an `operation` and a `target_type`
- [x] The four core built-in roles are present on every deployment and cannot be modified or
      deleted; the two integration roles appear only when the deployment opts in
- [x] What the core roles grant comes from configuration; an empty target list is refused and
      a list covering none of RBAC's own types is reported at startup
- [x] Configured startup grants are written at scope `/` on every boot, idempotently, and a
      grant naming an unseeded role aborts startup
- [x] A custom role is tenant-owned, uniquely named within its tenant, and cannot name an
      assignable scope outside that tenant's subtree
- [x] An assignment outside the role's assignable-scope trees is rejected
- [x] A family wildcard matches every concrete type inside the family; an operation wildcard
      matches every action; `gts.*` is not accepted
- [x] A `not_permissions` rule denies a request its own role would otherwise allow, and does
      not affect another role's grant
- [x] A tenant-scoped grant is inherited by descendant tenants; a resource-group-scoped grant
      is inherited within that group's subtree
- [x] Evaluation is order-independent, and reserved scope variants are never produced
- [x] Entity schemas are registered at startup with GTS-form `$id` values
- [x] An unauthenticated request is rejected before any permission evaluation
- [x] An authenticated call without the required permission returns a standardized,
      machine-readable error
- [x] The permission catalog is readable by any authenticated caller, filterable, and
      cursor-paginated
- [ ] An administrator can see and manage one principal's roles from that principal's own
      view

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| Types Registry | Entity-schema registration, `target_type` validation, permission-catalog instances | p1 |
| Tenant Resolver | Tenant existence and ancestry for scope validation and the ancestor walk | p1 |
| Resource Group gear | Group existence, tenant ownership, and membership for `Group` principals | p1 |
| PostgreSQL | Primary storage for role definitions and assignments | p1 |
| AuthN layer | Authenticated `SecurityContext` on every management-API call | p1 |
| Account Management | Display names for `User` principals and row authors. Resolved lazily from `ClientHub`, never declared in `deps` (that edge would close a dependency cycle); absence degrades to unnamed rows | p3 |
| Event Broker | Publication of the reserved mutation events | p3 (deferred) |

## 11. Assumptions

- Authenticated identities are supplied by the authentication layer; user and
  service-principal identity lifecycle stays outside this gear.
- Authoritative tenant and resource-group hierarchy providers exist and are reachable
  in-process.
- Domain gears enforce the authorization outcome at their own boundary; the gear does not
  replace gear-level enforcement.
- Built-in roles are seeded before normal administration begins, so platform bootstrap
  establishes a safe initial administration path.
- v1 targets management-plane authorization only; data-plane authorization is a later
  expansion.
- Runtime event publication is not a launch dependency; event contracts may be reserved now
  and activated later.
- `User` and `ServicePrincipal` identifiers are opaque to the gear in v1 and are matched
  verbatim against what the PDP presents.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| A consumer supplies the wrong tenant context to the in-process query | Silently narrowed scope walk producing a false denial, or a grant evaluated in the wrong branch | Only `ClientHub`-registered gears reach the contract; per-caller metrics and structured logs; every new consumer's derivation path is review-gated |
| Cross-domain `resource.type` taxonomy drifts between gears | Roles written against families that no longer match anything | Ownership defined per domain; the permission catalog publishes what actually exists; `target_type` is validated on write |
| Role explosion in large deployments | Administration becomes unmanageable | Built-in roles cover the common paths; custom-role growth is visible through the role-count metrics |
| Built-in role targets left at their defaults | `Contributor` and `Reader` authorize nothing, and `Owner` may not cover RBAC's own types | Startup warning when no entry covers RBAC's own types; empty lists refused; the operational consequence is documented in the gear README |
| Hierarchy or role data goes stale in the PDP's cache | An authorization decision made against a superseded hierarchy | TTL-bounded caching in the PDP; event-driven invalidation deferred with the Event Broker |
| Scope path format changes | Stored assignments become unparseable | Scope format is versioned and a migration path is documented before any change ships |
| Bootstrap fails on a fresh deployment | Nobody can administer the platform | Bootstrap is idempotent and gated by the readiness check; an unassertable grant fails the probe rather than starting half-configured |

## 13. Open Questions

| Question | Status | Notes |
|----------|--------|-------|
| Do we confirm grow-forward wildcard semantics as a team — a `*` rule silently granting resource types added by future releases? | TBD | The behaviour follows from family matching and is documented in `cpt-cf-rbac-fr-operation-wildcard`; what is outstanding is an explicit ruling that we want it. A "no" would mean pinning wildcard rules to the type set known at authoring time, changing both the engine and the authoring surface |
| How complete must the cross-domain `resource.type` taxonomy be before launch? | TBD | Ownership and release thresholds still need agreement |
| Is there a "my access" surface where a non-administrator sees their own effective roles? | TBD — deferred | Either scope the screen or drop the affordance; leaving a dangling action is the one outcome to avoid |
| Should a role definition carry an assignment count? | TBD | A per-role count is served today by a filtered assignment query, which is correct but chatty over a long list |
| How should hierarchy freshness be guaranteed across dependent components? | TBD | The design settles the mechanism once platform-wide freshness expectations exist |
| What is the long-term authorization-decision caching strategy? | Deferred in v1 | Revisit once invalidation and consistency requirements are defined |
| What is the scale strategy for very large hierarchies? | TBD | Revisit if projected hierarchy size exceeds the baseline design assumptions |
| Where are the per-principal role views built? | TBD | `cpt-cf-rbac-fr-principal-role-visibility` is served by the existing API; the presentation attaches to the user and user-group detail pages, which are designed outside this repository |
| Should the platform have its own Resource Group service? | Resolved | Yes — it ships as the [Resource Group gear](../../resource-group/docs/PRD.md) |
| How should the canonical operation and resource-type inventory be published? | Resolved | `GET /rbac/v1/permissions` publishes the permission catalog (`cpt-cf-rbac-fr-permission-catalog`) |

## 14. Traceability

- **Design**: [DESIGN.md](./DESIGN.md)
- **Downstream PDP**: [AuthZ Resolver Plugin PRD](../../authz-resolver/plugins/authz-resolver-plugin/docs/PRD.md),
  [AuthZ Resolver Plugin design](../../authz-resolver/plugins/authz-resolver-plugin/docs/DESIGN.md)
- **Gear operations**: [rbac/README.md](../rbac/README.md)
- **Entity schemas**: [schemas/](./schemas/)
- **ADRs**: none recorded for this gear; resolved design decisions are tabled in
  [DESIGN.md](./DESIGN.md#4-additional-context)
- **Features**: no feature specifications exist for this gear yet
