---
status: superseded
date: 2026-07-04
---

Created:  2026-07-07 by Virtuozzo International GmbH
Updated:  2026-07-07 by Virtuozzo International GmbH

# ADR-0002: Status-Driven Deprovisioning Saga with Name Retention


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Backend-first delete (no lifecycle state)](#backend-first-delete-no-lifecycle-state)
  - [Deprovisioning status releasing the name immediately](#deprovisioning-status-releasing-the-name-immediately)
  - [Deprovisioning status holding the name until cleanup](#deprovisioning-status-holding-the-name-until-cleanup)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-credstore-adr-deprovisioning-saga`

**Superseded by [ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md).** The deprovisioning saga and its name retention existed because a successor's value shared the deleted value's backend key; with immutable per-version keys the race it guarded against cannot occur, and deletion becomes one row transaction plus garbage collection.

## Context and Problem Statement

With a stateful gear ([ADR-0001](0001-cpt-cf-credstore-adr-stateful-gear.md)) a delete spans two stores: the metadata row and the backend value. The initial implementation deleted backend-first, then removed the row. Partial failures had no self-healing owner: a failed row delete left a value-less row (misleading 404s until a re-put), a crash between the two steps was repaired only by accident, and — unlike the create path — there was no lifecycle state a reaper could sweep. The create saga also carried a documented debt: a `mark_active` failure after a successful backend write orphaned the value in the backend forever.

How should deletion be structured so that revocation is atomic for readers, partial failures self-heal, and no failure mode can either resurrect a deleted secret or destroy a successor secret's value?

## Decision Drivers

* Revocation must be atomic from the reader's perspective — no window where a "deleted" secret still resolves
* Every partial-failure state must have an owner: client retry and/or background reaper
* The backend key for a reference is deterministic — a lagging cleanup step must never be able to delete a *successor* secret's value written under the same key
* Symmetry with the existing provisioning saga (one mental model, one reaper)
* Close the create-saga orphaned-backend-value debt

## Considered Options

* Keep the backend-first delete (no lifecycle state)
* `deprovisioning` status; release the unique index (the name) immediately at mark time
* `deprovisioning` status; the row keeps holding the unique index until backend cleanup completes

## Decision Outcome

Chosen option: "Deprovisioning status holding the name until cleanup" (the status set ships in the consolidated `m0001_initial_schema`). Delete is a saga symmetric to provisioning: flip the row to `deprovisioning` (version-gated when `If-Match` was given; the version is *not* bumped so a precondition retry still matches) → backend delete (`NotFound` = success) → row delete. From the flip onward the secret is invisible to resolution (single status filter — no read/delete race), while the row still holds the partial unique index, so re-creating the reference fails with a retryable conflict until cleanup finishes.

A stuck saga is resumed by either path: a `DELETE` retry (`find_own` returns `deprovisioning` rows and re-runs the idempotent steps) or the periodic reaper. The reaper also issues a best-effort backend delete for every stale row it sweeps — including stuck `provisioning` rows — which closes the orphaned-value debt of the create saga.

### Consequences

* Good, because revocation is atomic for readers and crash-safe: every intermediate state is non-readable and owned by retry or reaper
* Good, because name retention makes the "lagging cleanup deletes the successor's value" hazard structurally impossible (the successor cannot exist until the backend key is clean)
* Good, because one reaper loop and one status column serve both sagas; observability is symmetric (`provisioning_reaped` / `deprovisioning_reaped`, per-status inventory gauges)
* Bad, because a reference stays unavailable for re-creation for up to `deprovisioning_timeout_secs` + one reaper tick after a crashed delete (bounded, configurable)
* Bad, because a caller whose backend delete fails receives a retryable error for a secret that already no longer resolves — the 5xx reports cleanup state, not visibility

### Confirmation

* Service tests cover: backend failure leaves a non-resolving `deprovisioning` row; `DELETE` retry resumes; reaper completes stuck sagas and keeps rows while the backend still fails; provisioning-orphan reconciliation
* Repo tests confirm the `deprovisioning` row is invisible to `resolve_for_get`, visible to `find_own`, and still holds the unique index until reaped
* E2E `test_reference_reusable_after_delete` confirms the happy path releases the name immediately

## Pros and Cons of the Options

### Backend-first delete (no lifecycle state)

* Good, because fewest moving parts — two operations, no new status
* Bad, because partial failures have no owner: a value-less row yields misleading 404s until a client happens to re-put
* Bad, because nothing sweeps a crashed delete; asymmetric with the create saga
* Bad, because the caller error surface conflates "not deleted" with "half deleted"

### Deprovisioning status releasing the name immediately

* Good, because the reference becomes reusable the instant the delete is accepted (best availability)
* Bad, because it is unsafe: the old saga's lagging backend delete and the new secret's value share the same deterministic backend key — cleanup could erase the successor's value
* Bad, because preventing that requires per-write backend key versioning (a backend contract change) or cross-saga coordination

### Deprovisioning status holding the name until cleanup

* Good / Bad — see Decision Outcome and Consequences above

## Traceability

* **Design**: [DESIGN.md](../DESIGN.md) §6.1, §6.3, §6.4
* **Requirements**: `cpt-cf-credstore-fr-deprovisioning`, `cpt-cf-credstore-fr-write-lifecycle`
* **Related**: [ADR-0001](./0001-cpt-cf-credstore-adr-stateful-gear.md)
