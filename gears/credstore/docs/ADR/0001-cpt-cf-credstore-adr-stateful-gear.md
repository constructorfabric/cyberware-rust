---
status: accepted
date: 2026-07-04
---

Created:  2026-07-07 by Virtuozzo International GmbH
Updated:  2026-07-07 by Virtuozzo International GmbH

# ADR-0001: Stateful Gear with Gear-Owned Secret Metadata


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Stateless gear, metadata in the backend](#stateless-gear-metadata-in-the-backend)
  - [Stateful gear, value-only backend](#stateful-gear-value-only-backend)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-credstore-adr-stateful-gear`

## Context and Problem Statement

The original CredStore design was built on a **stateless gear**: all per-secret metadata (`sharing`, `owner_id`, `owner_tenant_id`) lived in the external backend and was returned on every `get`; secret identity was a deterministic ExternalID encoding `(tenant_id, key, owner_id) → base64url(...)`. Hierarchical resolution walked the tenant ancestor chain with up to N×2 backend round-trips (private probe + tenant/shared probe per level), authorization was a coarse permission-string check, and a private↔non-private sharing change had no atomic implementation ("no implicit migration guarantee"). Launching required the backend to first grow a three-value `sharing` enum and an `owner_id` column.

Where should secret metadata live, and which component owns policy, uniqueness, and identity?

## Decision Drivers

* Hierarchical resolution latency must not scale with tenant-hierarchy depth on the metadata side
* Tenant isolation must be enforceable at the data layer (platform PDP / SecureORM posture, consistent with RBAC/RG)
* No backend schema prerequisite — any dumb per-tenant key-value store should qualify as a backend plugin
* Encoded-ID collision surfaces must be designed out, not mitigated
* Writes spanning two stores need a recoverable failure story (crash-safety)
* Uniqueness rules (one non-private secret per `(tenant, ref)`; per-owner private secrets; coexistence of both classes under one reference) must be enforced somewhere authoritative

## Considered Options

* Stateless gear, metadata in the backend (original design)
* Stateful gear owning a metadata table, value-only backend

## Decision Outcome

Chosen option: "Stateful gear, value-only backend". The gear owns the `credstore_secrets` table (identity, sharing, ownership, lifecycle status, version); the backend plugin stores only the value, keyed by `(tenant_id, key, private-per-owner | tenant class)`. Almost every other design property follows from this single decision:

* uniqueness/coexistence rules become **partial unique indexes**;
* hierarchical resolution becomes **one indexed SQL query** over the ancestor chain plus at most **one** backend read for the winning row;
* authorization becomes a PDP `AccessScope` **enforced in SQL** through SecureORM clamps on the metadata table (fail-closed, anti-enumeration 404 preserved);
* secret identity is a DB row — the ExternalID encoding and its collision analysis disappear;
* writes become explicit **sagas** over the metadata row and backend value, with a lifecycle `status` column and a reaper (see [ADR-0002](0002-cpt-cf-credstore-adr-deprovisioning-saga.md));
* optimistic concurrency (`version` / `ETag` / `If-Match`) becomes possible at the metadata layer;
* the private↔non-private "migration" hazard is designed out: the two classes coexist under one reference and the transition is simply rejected as unsupported.

### Consequences

* Good, because walk-up latency no longer scales with hierarchy depth; the backend is touched at most once per resolution
* Good, because tenant isolation (PDP tenant-scope clamps) is enforced in SQL, not in application-level string checks
* Good, because backends need no metadata schema — the in-memory static plugin and any future vault plugin implement the same three-method value-store contract
* Good, because versioning, lifecycle statuses, and metadata-only future features (list, types) become cheap
* Bad, because the gear becomes a stateful gear: it needs a database, migrations, and the `stateful` capability
* Bad, because metadata and backend value can diverge transiently on partial failure — this cost is contained by the saga + reaper design ([ADR-0002](0002-cpt-cf-credstore-adr-deprovisioning-saga.md))

### Confirmation

* `credstore_secrets` schema with partial unique indexes ships in `m0001_initial_schema`; repo tests exercise uniqueness, coexistence, and scope clamps against SQLite
* Resolution is a single `resolve_for_get` query; the walk-up depth metric confirms no per-level backend calls
* E2E suite (`testing/e2e/suites/credstore/`) verifies hierarchical inheritance, shadowing, isolation, and optimistic concurrency through the REST API

## Pros and Cons of the Options

### Stateless gear, metadata in the backend

* Good, because the gear needs no database (no migrations, no stateful capability)
* Good, because secret data and metadata live in exactly one store (no divergence class of failures)
* Bad, because every resolution costs up to N×2 backend round-trips for an N-deep hierarchy
* Bad, because the backend must implement the metadata schema (`sharing`, `owner_id`) before the gear can launch, and every future metadata feature requires a backend change
* Bad, because identity-by-encoding carries a collision surface that must be validated and mitigated forever
* Bad, because authorization stays an application-level permission-string check — tenant-subtree scope cannot be expressed, and uniqueness rules are only as strong as each backend's guarantees
* Bad, because half-written state is invisible: with no gear state there is no notion of a recoverable in-flight write

### Stateful gear, value-only backend

* Good / Bad — see Decision Outcome and Consequences above

## More Information

This ADR condenses the historical `DESIGN-ADDENDUM.md`, which recorded the stateless→stateful delta section-by-section against the original (stateless) revision of `DESIGN.md`. The full comparison, including the original section references, is preserved in git history (`gears/credstore/docs/DESIGN-ADDENDUM.md` prior to its removal).

## Traceability

* **Design**: [DESIGN.md](../DESIGN.md) §1, §3.1, §4.7
* **Requirements**: `cpt-cf-credstore-fr-hierarchical-resolve`, `cpt-cf-credstore-fr-authz-pdp`, `cpt-cf-credstore-nfr-tenant-isolation`
* **Related**: [ADR-0002](./0002-cpt-cf-credstore-adr-deprovisioning-saga.md)
