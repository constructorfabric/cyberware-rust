---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-06-10 by Virtuozzo International GmbH

# Independent major-version stability for REST, SDK, and Plugin SPI

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Independent major-version stability per surface](#independent-major-version-stability-per-surface)
  - [Single shared version across surfaces](#single-shared-version-across-surfaces)
  - [Calendar-versioned release train](#calendar-versioned-release-train)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-contract-stability`

## Context and Problem Statement

The Usage Collector exposes three public surfaces — the REST API for remote callers, the in-process SDK trait for platform gears, and the Plugin SPI for storage backends — and each surface has a different ecosystem of authors and release schedules. The question is how these surfaces should version: a single coupled version across all three, fully independent versioning, or a coordinated release train. The decision affects how plugin authors, in-process consumers, and remote callers experience compatibility, how breaking changes are introduced, and how long an ecosystem participant has to migrate across a major-version step.

## Decision Drivers

- `cpt-cf-usage-collector-nfr-plugin-contract-stability` — SDK trait, Plugin SPI, and REST API each remain stable within a major version.
- PRD §1.3 ecosystem-decoupling goal (centralized metering plus operator-self-service) — release cadences of plugin authors and downstream consumers must not be coupled to the core's release train.
- `cpt-cf-usage-collector-principle-contract-stability` — codifies major-version stability as a first-class principle.
- `cpt-cf-usage-collector-nfr-plugin-contract-stability` (PRD §6.1) backwards-compatibility expectations — additive changes only within a major version; coexistence window across one prior major.

## Considered Options

- Independent major-version stability per surface — each surface (REST, SDK, Plugin SPI) versions on its own track; within a major version only additive changes are permitted; the platform supports at most one prior major version per surface concurrently.
- Single shared version across surfaces — all three surfaces share one major-version number that advances together.
- Calendar-versioned release train — surfaces ship together on a fixed schedule (e.g., quarterly) with semver-style additive vs breaking change labels per release.

## Decision Outcome

Chosen option: "Independent major-version stability per surface", because it is the only option that decouples plugin-author, in-process consumer, and remote-caller release schedules from each other and from the Usage Collector's own release train. Each surface versions independently; within a major version only additive changes are permitted; the platform supports at most one prior major version per surface concurrently, giving every ecosystem participant a clear migration window. Breaking changes on any one surface advance that surface's major version without forcing the other surfaces to advance.

### Consequences

- Plugin authors can ship a release built against Plugin SPI `N` and continue to function unchanged against every `N.x` minor and patch release of the core.
- In-process SDK consumers and remote REST callers each see their own surface's compatibility envelope and migrate independently.
- The platform commits to running two concurrent major versions of any one surface during the migration window; this is bounded to one prior major to keep the support matrix tractable.
- Breaking changes are expressed as a new major version that coexists with the prior; mid-major breaking changes are not permitted.
- The release process must publish per-surface compatibility envelopes and migration guides each major version step.

### Confirmation

Compliance is confirmed through (a) contract compatibility tests gating every release — compile-time tests for SDK and Plugin SPI, schema-diff tests for REST — against the prior major version; (b) review of the §3.3 API endpoints stability column to ensure each surface's version trajectory is recorded; and (c) release-process review confirming per-surface major-version coexistence is supported by deployment tooling.

## Pros and Cons of the Options

### Independent major-version stability per surface

Each surface versions on its own track; additive within a major; one prior major supported concurrently.

- Good, because plugin authors and in-process consumers migrate on schedules that fit their own release cadences.
- Good, because breaking changes on one surface do not force breaking changes on the others, reducing churn.
- Good, because the compatibility envelope per surface is explicit and machine-checkable through contract tests.
- Neutral, because the support matrix has up to two concurrent majors per surface; this is bounded and operationally tractable.
- Bad, because cross-surface refactors that would naturally span multiple surfaces require careful per-surface staging; the cost is higher than a shared-version refactor.

### Single shared version across surfaces

All three surfaces share one major-version number that advances together.

- Good, because the release artifact is a single coherent version; documentation and migration guides are unified.
- Bad, because a breaking change on one surface forces every other surface into a new major even when its contract is unchanged.
- Bad, because plugin authors and remote callers are forced into release cycles tied to the most-churn-prone surface.
- Bad, because the migration window doubles in pressure: every ecosystem participant must migrate concurrently rather than on their own track.

### Calendar-versioned release train

Surfaces ship together on a fixed schedule (e.g., quarterly), with per-release labels for additive vs breaking.

- Good, because there is a predictable cadence and a fixed planning horizon.
- Bad, because the surfaces still travel together, importing the same coupling problems as the shared-version option.
- Bad, because calendar releases do not match the natural cadence of plugin-author or remote-caller ecosystems.
- Bad, because breaking changes occur as a function of calendar position rather than ecosystem readiness.

## More Information

Related decisions: `cpt-cf-usage-collector-adr-pluggable-storage` (the Plugin SPI whose stability this ADR governs). The §3.3 REST Endpoints Overview stability column and the §3.3 Plugin SPI / SDK trait subsections are the structural anchors.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-nfr-plugin-contract-stability` — major-version stability on each public surface.
- `cpt-cf-usage-collector-principle-contract-stability` — codifies the principle in §2.1.
- `cpt-cf-usage-collector-constraint-plugin-contract-stability` — the §2.2 constraint that pairs with this ADR.
- `cpt-cf-usage-collector-interface-rest-api`, `cpt-cf-usage-collector-interface-sdk-client`, `cpt-cf-usage-collector-interface-plugin` — the three surfaces governed by this decision.
