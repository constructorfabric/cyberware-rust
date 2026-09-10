---
status: accepted
date: 2026-07-21
decision-makers: Constructor Fabric Steering Committee
---

Created:  2026-07-22 by Virtuozzo International GmbH
Updated:  2026-07-22 by Virtuozzo International GmbH

# ADR-0008: Tenant-Scoped User Attribute Update as an IdP Pass-Through

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Option 1: Pure IdP pass-through](#option-1-pure-idp-pass-through)
  - [Option 2: Document the IdP admin API as the only path](#option-2-document-the-idp-admin-api-as-the-only-path)
  - [Option 3: AM-side user projection backing the update](#option-3-am-side-user-projection-backing-the-update)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-account-management-adr-user-attribute-update`

## Context and Problem Statement

The AM↔IdP user-operations contract shipped with exactly three operations —
`provision_user`, `deprovision_user`, `list_users` — enumerated in
[ADR-0001](0001-cpt-cf-account-management-adr-idp-contract-separation.md),
[ADR-0005](0005-cpt-cf-account-management-adr-idp-user-identity-source-of-truth.md),
PRD §5.5, DECOMPOSITION §2.5, and `feature-idp-user-operations-contract`. There
is no way for a tenant administrator to change a provisioned user's mutable
attributes (contact email, display name, given/family name, login username, or
password) through AM. The only documented path is to call the configured IdP's
admin API directly, which bypasses AM's tenant-scoped authorization gate, its
GTS structural validation, and its audit emission.

We need to decide whether — and how — to expose user attribute update through
AM without violating the source-of-truth boundary established by ADR-0005.

The concrete question is narrow: **does adding an `update_user` operation
require AM to store or cache user state?** If not, the change is additive to a
stable contract and consistent with the existing architecture.

## Decision Drivers

* **Source-of-truth invariant (ADR-0005)**: the IdP remains the sole
  authoritative store for user identity; AM persists no local user table,
  projection, or cache (`cpt-cf-account-management-constraint-no-user-storage`).
* **Uniform administrative surface**: create and delete already flow through
  AM's tenant-scoped, authorized, audited endpoints; attribute update is the
  missing verb in that lifecycle and should share the same posture.
* **Contract stability**: the SDK trait (`IdpPluginClient`) and the SDK client
  (`AccountManagementClient`) are stable interfaces; within a major version
  only additive changes are permitted (DESIGN §2.2).
* **Legacy-provider tolerance**: not every conforming IdP can mutate every
  attribute; the contract must let a provider decline without breaking AM.
* **PII minimization (GDPR)**: an update path must not become a reason for AM
  to start persisting profile data.

## Considered Options

1. **Expose attribute update as a pure IdP pass-through** — add
   `update_user` to `IdpPluginClient` and a `PATCH
   /tenants/{tenant_id}/users/{user_id}` REST operation that validates and
   forwards a JSON Merge Patch to the IdP, persisting nothing.
2. **Do not expose update; document the IdP admin API as the only path** —
   status quo. Attribute edits go directly to the provider, outside AM's
   authorization/validation/audit envelope.
3. **Expose update but back it with an AM-side user projection** — mirror user
   attributes locally to serve reads and diff updates. Rejected on sight
   because it reverses ADR-0005.

## Decision Outcome

Chosen option: **Option 1 — expose attribute update as a pure IdP
pass-through**, because it closes the lifecycle gap while leaving the ADR-0005
no-storage invariant fully intact. `update_user` is a fourth pass-through
operation, symmetrical with `provision_user`/`deprovision_user`: AM validates
the request, forwards it to the IdP through `ClientHub`, projects the
provider's response through `gts.cf.core.am.user.v1~`, and persists nothing.

Decisions settled within Option 1:

* **Mutable attribute set**: `username`, `email`, `display_name`,
  `first_name`, `last_name`, and `password`. The **user-tenant binding
  attribute is explicitly excluded** — user-tenant reassignment remains a v1
  non-goal (PRD §1.4 / §13,
  [ADR-0006](0006-cpt-cf-account-management-adr-idp-user-tenant-binding.md)),
  so this ADR does not touch the tenant-identity attribute.
* **PATCH, not PUT**: partial update via **JSON Merge Patch (RFC 7396)** per
  `guidelines/DNA/REST/API.md`. An omitted field is unchanged; an explicit
  `null` clears a nullable profile field; a value sets it.
* **`username` cannot be cleared**: the login identifier is `required` in the
  published schema, so an explicit `null` on `username` is rejected with
  `code=validation`; a value renames it (uniqueness collisions surface as
  `409 already_exists`).
* **Absence is a 404, not idempotent success**: unlike `deprovision_user`
  (which folds a vendor "already gone" response into success), `update_user`
  surfaces `IdpUserOperationFailure::NotFound` → `404`.
* **`password` is write-only**: it sets a credential at the IdP and is never
  part of the user projection or any response body.

### Consequences

* `IdpPluginClient` gains a fourth user method with a default implementation
  that returns `idp_unsupported_operation`, so existing tenant-only or
  read-only adapters compile unchanged and legacy providers that cannot mutate
  attributes decline explicitly (they MUST NOT silently no-op, per ADR-0001).
* AM adds no table, migration, or projection. Every update is a live call to
  the IdP; an IdP outage fails the update with `idp_unavailable` rather than
  degrading to cached state — identical to the other user operations.
* The change is additive within `v1`: a new HTTP method on an existing
  resource plus a new optional SDK/trait method (DESIGN §2.2;
  `guidelines/DNA/REST/VERSIONING.md`). No `v2`.
* Conforming provider adapters (Keycloak, Zitadel, Dex, …) shipped outside
  this gear must implement `update_user` to support the feature; until they do
  they return `501`.
* Profile-mutation guidance that previously said "edits go directly to the
  provider admin API" is superseded for the enumerated attribute set.

### Confirmation

* Code review verifies `update_user` writes no AM-side row and calls the IdP
  through `ClientHub` (no direct provider-library use).
* Unit + integration tests confirm: PATCH is authorized and tenant-scoped;
  JSON Merge Patch null-clears a nullable field; a `username` rename collision
  is `409`; an absent user is `404`; an empty patch is `400`; an IdP outage is
  `503`; and an unsupported provider is `501`.
* The SemVer contract-check gate confirms the OpenAPI + SDK + IdP-trait changes
  are additive (`feature-errors-observability` versioning discipline).

## Pros and Cons of the Options

### Option 1: Pure IdP pass-through

* Good, because it preserves ADR-0005 (no local user storage) verbatim.
* Good, because it reuses the existing authorize → resolve-tenant → validate →
  forward saga and audit envelope, matching create/delete exactly.
* Good, because it is additive and IdP-agnostic (default impl declines).
* Bad, because a working end-to-end experience depends on each provider adapter
  implementing the new method.

### Option 2: Document the IdP admin API as the only path

* Good, because it requires no code change.
* Bad, because attribute edits bypass AM's authorization, GTS validation, and
  audit trail — inconsistent with how create and delete are governed.
* Bad, because it pushes provider-specific admin-API knowledge onto every
  operator and tool.

### Option 3: AM-side user projection backing the update

* Good, because it could serve reads without the IdP.
* Bad, because it reverses ADR-0005, reintroduces sync/consistency complexity,
  and expands the persisted-PII surface. Rejected.

## More Information

The pass-through posture is the same one ADR-0005 already describes for
provision/deprovision ("pure pass-through to the `IdpPluginClient` contract").
ADR-0006 anticipated a future attribute-mutation shape (tenant reassignment)
routed "through the IdP contract" with the IdP staying canonical; this ADR
applies that pattern to profile/credential/username attributes while keeping
the tenant-binding attribute out of scope.

## Traceability

- **PRD**: [PRD.md](../PRD.md) §5.5 (`cpt-cf-account-management-fr-idp-user-update`)
- **DESIGN**: [DESIGN.md](../DESIGN.md) §2.2 (additive-only within a version), §3.3 (`IdpPluginClient` surface)
- **Feature**: [feature-idp-user-operations-contract.md](../features/feature-idp-user-operations-contract.md)
- **Related**: [ADR-0001](0001-cpt-cf-account-management-adr-idp-contract-separation.md) (trait method set), [ADR-0005](0005-cpt-cf-account-management-adr-idp-user-identity-source-of-truth.md) (source-of-truth boundary preserved), [ADR-0006](0006-cpt-cf-account-management-adr-idp-user-tenant-binding.md) (tenant-binding attribute excluded)

This decision directly addresses the following requirements and design elements:

* `cpt-cf-account-management-fr-idp-user-update` — attribute update through the IdP integration contract.
* `cpt-cf-account-management-principle-idp-agnostic` — update goes through `IdpPluginClient`; no hard-coded provider logic.
* `cpt-cf-account-management-constraint-no-user-storage` — update persists no AM-side user state.
