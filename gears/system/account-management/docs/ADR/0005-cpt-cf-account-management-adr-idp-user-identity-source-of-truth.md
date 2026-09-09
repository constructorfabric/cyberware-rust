---
status: accepted
date: 2026-04-09
decision-makers: Constructor Fabric Steering Committee
---

Created:  2026-03-31 by Virtuozzo International GmbH
Updated:  2026-07-22 by Virtuozzo International GmbH

# ADR-0005: IdP as the Single Source of Truth for User Identity Data


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Option 1: IdP as sole source of truth — no local user table](#option-1-idp-as-sole-source-of-truth--no-local-user-table)
  - [Option 2: Local user table synchronized from IdP](#option-2-local-user-table-synchronized-from-idp)
  - [Option 3: Local user table as primary store with IdP for authentication only](#option-3-local-user-table-as-primary-store-with-idp-for-authentication-only)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-account-management-adr-idp-user-identity-source-of-truth`

## Context and Problem Statement

AM coordinates user lifecycle operations (provisioning, deprovisioning, tenant-scoped queries) and needs access to user identity information — credentials, profile attributes, authentication state, and user existence. The platform must decide where this data is authoritatively stored: in the IdP, in AM's own database, or in both. This decision has significant implications for data consistency, operational complexity, GDPR compliance, and system availability during IdP outages.

The Gears platform uses a pluggable IdP integration model (see `cpt-cf-account-management-adr-idp-contract-separation`). Different deployments may use Keycloak, Azure AD, Okta, or custom identity providers. AM must work consistently regardless of which IdP backs the deployment.

## Decision Drivers

* **Single source of truth for identity**: User identity data (credentials, profile, authentication state) must have exactly one authoritative store to avoid split-brain inconsistencies between AM and the IdP.
* **Data minimization (GDPR)**: AM acts as a data processor. Storing user PII locally increases the regulatory surface and requires additional data protection controls, retention policies, and right-to-erasure compliance at the gear level.
* **IdP-agnostic architecture**: AM must work with any conforming IdP implementation. A local user table would create a second identity store that must be kept in sync with each IdP variant, coupling AM to provider-specific synchronization protocols (SCIM, webhooks, polling).
* **Operational simplicity**: Synchronization between two identity stores introduces eventual consistency windows, conflict resolution logic, and a new failure mode class (sync lag, missed events, stale projections).
* **Availability during IdP outages**: Without a local cache, user operations fail when the IdP is unreachable. The platform must accept this trade-off or add caching complexity.

## Considered Options

1. **IdP as sole source of truth — no local user table**: AM stores no user identity data. All user existence checks, profile lookups, and tenant-scoped queries are delegated to the IdP via the `IdpPluginClient` contract at operation time. User identifiers appear in AM only as IdP-issued UUID references in audit logs and as arguments passed to Resource Group.
2. **Local user table synchronized from IdP**: AM maintains a local projection of user records (identity, profile, tenant binding) synchronized from the IdP via periodic polling or event-driven updates. The IdP remains the primary store; AM's table is a read-optimized cache.
3. **Local user table as primary store with IdP for authentication only**: AM owns user identity data in its own database. The IdP is used solely for credential validation and token issuance. AM provisions users locally and pushes credential setup to the IdP.

## Decision Outcome

Chosen option: **Option 1 — IdP as sole source of truth**, because it enforces a single authoritative store for user identity, eliminates synchronization complexity across pluggable IdP implementations, minimizes persisted PII (satisfying GDPR data minimization), and keeps AM focused on tenant hierarchy administration rather than identity management.

AM references users exclusively by IdP-issued UUID user identifiers. It does not cache, project, or independently assert user existence or profile data. If the IdP is unavailable, user operations fail with `idp_unavailable` rather than degrading to potentially stale cached state.

### Consequences

* AM has no `users` table and no user-related database entities. User-facing operations (provision, deprovision, list) are pure pass-through to the `IdpPluginClient` contract, reducing AM's schema surface and migration burden.
* User operations are unavailable when the IdP is unreachable. The platform accepts this trade-off: tenant hierarchy reads and non-IdP-dependent admin operations (tenant CRUD, metadata resolution, children queries, status changes) continue to function during IdP outages.
* No synchronization protocol is required between AM and the IdP. This eliminates eventual consistency windows, missed-event recovery, and provider-specific sync adapters — critical for maintaining IdP-agnostic portability.
* GDPR data processor obligations at the AM gear level are minimal: AM processes identity-linked payloads transiently during API calls but does not persist them. Right-to-erasure requests are handled entirely by the IdP.
* Group membership links in Resource Group reference users by UUID identifier. If a user is deprovisioned from the IdP, the membership link becomes orphaned. Orphan detection requires future cross-gear coordination (deferred from v1).
* Any future need for local user search or analytics (e.g., "find all users across all tenants matching a name pattern") must be satisfied by the IdP's query capabilities or by a separate reporting/analytics service — AM cannot provide these independently.

### Confirmation

* DESIGN and code review verify that AM's database schema contains no user-related tables or entities.
* Integration tests confirm that user operations fail with `idp_unavailable` when the IdP mock is unreachable, and that tenant hierarchy operations remain available.
* Code review verifies that user identifiers in AM code are treated as UUID strings rather than provider-specific opaque tokens, with no deserialization into profile structures.

## Pros and Cons of the Options

### Option 1: IdP as sole source of truth — no local user table

* Good, because it enforces a single authoritative store — no split-brain risk for user identity data.
* Good, because it eliminates synchronization complexity across pluggable IdP implementations.
* Good, because it minimizes persisted PII, satisfying GDPR data minimization and reducing the gear-level regulatory surface.
* Good, because it keeps AM's database schema focused on tenant hierarchy, reducing migration and maintenance burden.
* Good, because it maintains IdP-agnostic portability — no provider-specific sync protocols required.
* Bad, because user operations are unavailable during IdP outages (no graceful degradation for user queries).
* Bad, because AM cannot independently answer user-related queries without calling the IdP.

### Option 2: Local user table synchronized from IdP

* Good, because user queries can be served locally, improving latency and enabling availability during short IdP outages.
* Good, because it provides a local index for cross-referencing users with tenant hierarchy data.
* Bad, because it introduces a synchronization protocol between AM and each IdP implementation, coupling AM to provider-specific event/polling mechanisms.
* Bad, because eventual consistency windows mean the local table may contain stale data (deprovisioned users still appearing, new users missing).
* Bad, because it increases the persisted PII surface, requiring additional GDPR controls (retention, erasure) at the gear level.
* Bad, because conflict resolution between the local table and IdP (e.g., concurrent updates) adds operational complexity.

### Option 3: Local user table as primary store with IdP for authentication only

* Good, because AM has full control over user data and can serve queries without external dependencies.
* Good, because it eliminates reliance on IdP query capabilities.
* Bad, because it makes AM an identity store, fundamentally changing its role from tenant hierarchy administrator to identity manager.
* Bad, because it requires bidirectional synchronization with the IdP for credential management, creating tight coupling.
* Bad, because it maximizes persisted PII and GDPR regulatory surface at the gear level.
* Bad, because it duplicates identity management functionality that the IdP already provides, violating the platform's separation of concerns.

## More Information

The IdP-agnostic principle (`cpt-cf-account-management-principle-idp-agnostic`) and the IdP contract separation decision (`cpt-cf-account-management-adr-idp-contract-separation`) establish the architectural context for this ADR. The `IdpPluginClient` trait defines the boundary through which AM interacts with user identity — `provision_user`, `update_user` (added by `cpt-cf-account-management-adr-user-attribute-update`), `deprovision_user`, and `list_users` are the only user data access paths. Each is a pure pass-through: this decision's no-local-storage invariant holds unchanged across all of them.

PRD Section 3.4 (User Data Ownership) defines the ownership boundaries that this ADR formalizes as an architectural decision.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **Related**: [ADR-0006](0006-cpt-cf-account-management-adr-idp-user-tenant-binding.md) — ADR-0005 establishes that AM maintains no local user table; ADR-0006 decides where the user-tenant binding lives within that constraint. Decided together on the same date.
- **Extended by**: [ADR-0008](0008-cpt-cf-account-management-adr-user-attribute-update.md) — adds `update_user` as a fourth pass-through user operation; the no-local-storage invariant established here is preserved unchanged.

This decision directly addresses the following requirements and design elements:

* `cpt-cf-account-management-principle-idp-agnostic` — This ADR is the architectural rationale for why AM never stores user identity data locally, formalizing the IdP-agnostic principle's "no local user table" constraint.
* `cpt-cf-account-management-constraint-no-user-storage` — This ADR provides the decision rationale for the "No Direct User Data Storage" constraint in the DESIGN.
* `cpt-cf-account-management-fr-idp-user-provision` — User provisioning operates as pure pass-through to IdP; no local record is created.
* `cpt-cf-account-management-fr-idp-user-deprovision` — User deprovisioning delegates entirely to IdP; no local cleanup needed beyond orphaned group membership references.
* `cpt-cf-account-management-fr-idp-user-query` — User queries are delegated to IdP at operation time; no local projection is available as a fallback.
* `cpt-cf-account-management-constraint-data-handling` — Data minimization (no persisted user PII) directly supports GDPR processor obligations at the gear level.
