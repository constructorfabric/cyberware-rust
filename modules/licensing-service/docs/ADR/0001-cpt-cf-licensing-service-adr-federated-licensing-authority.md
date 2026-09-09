---
status: proposed
date: 2026-04-02
decision_owner: Platform Engineering
approver: Architecture Review Board
scope: licensing-service; all ModKit modules integrating licensing and quota enforcement
---

# ADR-0001: Federated Licensing Authority Model for ModKit and Legacy VendorA Integration

**ID**: `cpt-cf-licensing-service-adr-federated-licensing-authority`

## Context and Problem Statement

ModKit and CyberFabric modules need a single clean licensing interface for feature gating, entitlement lookup, effective limits, and quota-aware decisions. At the same time, some deployments run inside a legacy VendorA data center where commercial licensing truth already exists in VendorA licensing systems.

The core problem is how to introduce a new licensing capability for ModKit modules without creating double business truth, forcing direct legacy coupling into every module, or blocking future migration to a native platform licensing model.

## Decision Drivers

- New ModKit modules need one canonical licensing API and SDK
- Legacy VendorA licensing remains authoritative for some products, tenants, or entitlement scopes
- A single tenant or product scope must have exactly one authoritative licensing source at a time
- Runtime quota enforcement requires low-latency decisioning that should not depend on synchronous legacy round-trips on every hot-path request
- The platform must support gradual migration from legacy-authoritative to native-authoritative licensing without large-bang cutover
- The platform must provide explainable outcomes identifying which authority limited or denied an operation
- The platform must avoid leaking legacy VendorA domain quirks into every ModKit module

## Considered Options

- Direct module-to-legacy integration
- Immediate full replacement of legacy licensing with a new ModKit licensing source of truth
- Federated licensing facade with authority routing and anti-corruption adapters

## Decision Outcome

Chosen option: **Federated licensing facade with authority routing and anti-corruption adapters**.

The platform will expose one ModKit licensing interface to modules. Internally, the licensing service will route decisions to the correct authority per product, offering item, tenant scope, or deployment mode.

The authoritative source for a given licensing scope may be:

- Legacy VendorA licensing
- Native ModKit licensing
- Shadow mode for migration analysis only

The licensing service will normalize authority-specific concepts into a canonical platform model and provide a stable API for modules. Modules must not call legacy VendorA licensing APIs directly.

### Consequences

- Good, because all modules depend on one canonical licensing API instead of embedding legacy-specific integrations
- Good, because legacy VendorA licensing can remain authoritative for legacy-controlled entitlements during migration
- Good, because new ModKit-native modules can later become native-authoritative without changing consuming module APIs
- Good, because the platform can run shadow comparisons between legacy and native decisions before cutover
- Good, because the licensing service can provide explanation metadata identifying whether rejection or limitation came from legacy entitlement, native entitlement, or runtime quota enforcement
- Good, because the anti-corruption layer isolates VendorA-specific concepts such as license-key lifecycle, provisioning quirks, and offering-item semantics from module business logic
- Bad, because the licensing service becomes a critical integration boundary and must own routing, normalization, sync, and failure semantics
- Bad, because projections and caches can become stale and require explicit reconciliation strategy
- Bad, because the platform must maintain authority-routing configuration and migration discipline

### Confirmation

- Code review: verify ModKit modules use the licensing-service SDK rather than direct legacy calls
- Integration tests: verify the same entitlement scope cannot be configured as authoritative in both legacy and native modes simultaneously
- Contract tests: verify legacy adapter responses are normalized into canonical ModKit entitlement, feature, limit, and explanation models
- Resilience tests: verify cached read models and degraded-mode behavior follow documented fail-open or fail-closed rules per operation type
- Migration tests: verify shadow mode records comparison results without affecting authoritative decisions

## Pros and Cons of the Options

### Direct module-to-legacy integration

Each ModKit module calls legacy VendorA licensing APIs directly and interprets legacy responses itself.

- Good, because no new facade service is required initially
- Good, because legacy VendorA remains the only obvious source of truth
- Bad, because every module becomes coupled to legacy VendorA protocols, terms, and failure modes
- Bad, because migration to native licensing later requires touching every consumer module
- Bad, because there is no single place to normalize explanation, caching, authority routing, or degraded behavior

### Immediate full replacement of legacy licensing with a new ModKit licensing source of truth

A new ModKit licensing service becomes authoritative immediately for all products and scopes and imports or rewrites legacy state during migration.

- Good, because the target architecture is clean once completed
- Good, because there is only one authority after cutover
- Bad, because the migration blast radius is large and risky
- Bad, because hybrid deployments inside legacy VendorA data centers still need interoperability during transition
- Bad, because the new system must reproduce all legacy commercial and provisioning edge cases before go-live

### Federated licensing facade with authority routing and anti-corruption adapters

Modules call a canonical ModKit licensing API. The service resolves authority per scope, normalizes responses, and integrates with runtime quota enforcement.

- Good, because modules stay isolated from legacy specifics
- Good, because one API supports both legacy-authoritative and native-authoritative deployments
- Good, because migration can proceed incrementally using authority modes and shadow comparisons
- Good, because the service can combine business entitlement decisions with runtime quota signals in a controlled, explainable way
- Bad, because this adds routing, projection, reconciliation, and adapter complexity to the platform
- Bad, because incorrect authority configuration can cause subtle entitlement bugs if not validated strictly

## More Information

### Canonical Ownership Model

- **Licensing Service** owns canonical licensing and quota APIs, authority routing, normalized entitlement read models,
  explanation of business entitlement outcomes, quota policy resolution, runtime enforcement semantics, reservation
  lifecycle, usage accounting, and reconciliation
- **Legacy VendorA licensing** may remain authoritative for commercial truth for legacy-controlled scopes
- **Native ModKit licensing** may become authoritative for new platform-native scopes

Internally, the module maintains two layers:
- **Entitlement layer** owns business truth, authority routing, federation, shadow comparison, and legacy adaptation
- **Quota layer** owns policy resolution, runtime enforcement, reservation lifecycle, usage accounting, and reconciliation

### Authority Modes

Each licensing scope must operate in one explicit mode:

- `legacy_authoritative`
- `native_authoritative`
- `shadow`
- `disabled`

A licensing scope is typically keyed by product family, offering item, tenant scope, deployment environment, or an equivalent routing key defined by the platform.

### Anti-Corruption Layer

The legacy adapter must translate legacy VendorA concepts into canonical platform concepts instead of exposing raw legacy structures to modules.

Canonical concepts include:

- `Feature`
- `ResourceClass`
- `OfferingItem`
- `EntitlementGrant`
- `EffectiveLimit`
- `UsageSnapshot`
- `DecisionExplanation`

### Relation to Quota Enforcement

Quota enforcement is unified into the Licensing Service module rather than being a separate service. The former
standalone `quota-service` module has been removed from the repository; its requirements are incorporated into the
Licensing Service PRD (§5.5–§5.11).

The unification decision itself, including considered alternatives and consequences, is recorded in
[ADR-0002 Unified Licensing and Quota Module](./0002-cpt-cf-licensing-service-adr-unified-licensing-quota-module.md).
This ADR (ADR-0001) covers only the federated authority model.

The quota layer does not own commercial licensing truth. Internal ownership boundaries ensure that the entitlement
layer remains authoritative for business truth while the quota layer owns runtime enforcement, reservation lifecycle,
and usage accounting.

### Failure Policy

For legacy-authoritative scopes, cached entitlement data classified as `freshness_state ∈ { fresh, stale_within_policy }` (per `cpt-cf-licensing-service-fr-freshness-semantics`) **MAY** be used where the per-scope `degradation_policy` permits; data classified as `freshness_state = stale_beyond_policy` **MUST NOT** be treated as authoritative, and its use (if any) is governed solely by the per-scope `degradation_policy ∈ { fail_open, fail_closed, emergency_fallback }` defined in `cpt-cf-licensing-service-fr-safe-degradation`. Operation-class eligibility for degraded evaluation is governed by the closed whitelist in `cpt-cf-licensing-service-fr-degraded-policy`; operations outside that whitelist (including state-changing admission operations such as `CheckAndReserve`, `Commit`, `AcquireLease`) **MUST** return a service error instead of a degraded success result when dependencies are unavailable. The `fail_closed` default for `degradation_policy` (when unset) is established by `cpt-cf-licensing-service-fr-safe-degradation`; this ADR does not introduce an independent default.

### Rollout Strategy

- Phase 1: facade only, legacy-authoritative
- Phase 2: shadow comparisons between legacy and native models
- Phase 3: hybrid enforcement with legacy business truth and the internal quota layer of Licensing Service
- Phase 4: native-authoritative for selected new platform-native modules

## Review Cadence

Revisit this decision when one of the following becomes true:

- Legacy VendorA licensing is no longer required for any production deployment
- The platform needs multi-authority composition within the same entitlement scope, which would invalidate the one-authority-per-scope rule
- Runtime latency or reconciliation costs indicate the need to split facade responsibilities further

## Traceability

- **PRD**: [../PRD.md](../PRD.md)
- **Companion ADR**: [0002-cpt-cf-licensing-service-adr-unified-licensing-quota-module.md](./0002-cpt-cf-licensing-service-adr-unified-licensing-quota-module.md)
- **Superseded Module**: former standalone `quota-service` module; removed from the repository, requirements incorporated into §5.5–§5.11 of Licensing Service PRD

This decision directly addresses the following planned requirements or design elements:

- `cpt-cf-licensing-service-fr-canonical-api`
- `cpt-cf-licensing-service-fr-authority-routing`
- `cpt-cf-licensing-service-fr-legacy-adapter`
- `cpt-cf-licensing-service-fr-explanation`
