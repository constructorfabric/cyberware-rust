---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

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

The gear exposes three public surfaces: the REST API for remote callers, the
in-process SDK trait for platform gears, and the Plugin SPI for storage backends.
Each surface has its own ecosystem of authors and its own release schedule.

The question is how the three surfaces version. One coupled version can span all
three, each surface can version on its own, or all three can ride a coordinated
release train. The answer decides how each of the three ecosystems experiences
compatibility, and how long it has to migrate across a major-version step.

No surface has reached its 1.0 release, and the gear has no installations and
no consumers. So this decision must also fix when the contract starts to
apply.

## Decision Drivers

- `cpt-cf-usage-collector-nfr-plugin-contract-stability` — each public surface
  stays stable within a major version. Only additive changes are allowed inside a
  major, and at most one prior major is supported at a time.
- Ecosystem decoupling, recorded in that NFR's rationale — plugin authors,
  consumer gears, and remote usage sources are rarely the gear's maintainers.
  Their release cadence must not follow the gear's.
- `cpt-cf-usage-collector-principle-contract-stability` — major-version stability
  is a first-class design principle.
- Pre-1.0 status — a guarantee with no consumer to protect adds release work
  and blocks the design changes that early-stage work needs.

## Considered Options

- Independent major-version stability per surface — each surface versions on its
  own track. Only additive changes are allowed inside a major version, and at
  most one prior major is supported at a time.
- Single shared version across surfaces — all three surfaces share one
  major-version number that advances together.
- Calendar-versioned release train — the surfaces ship together on a fixed
  schedule, for example quarterly, with an additive-or-breaking label per
  release.

## Decision Outcome

Chosen option: "Independent major-version stability per surface". It is the only
option that leaves each ecosystem's release schedule independent of the others
and of the gear's own release train.

One prior major version per surface stays supported, which gives every ecosystem
participant a bounded migration window. A breaking change on one surface advances
that surface's major version alone, and the other two stay where they are.

The contract starts at the 1.0 release of each surface, and each surface
reaches 1.0 on its own schedule. Before that release a breaking change ships in
place, because no consumer exists to migrate.

### Consequences

- A plugin built against Plugin SPI major version `N` keeps working unchanged
  against every `N.x` minor and patch release of the gear.
- In-process SDK consumers and remote REST callers each see the compatibility
  envelope of their own surface, and migrate independently.
- The platform runs at most two concurrent major versions of a surface, which
  keeps the support matrix small.
- A breaking change appears as a new major version alongside the prior one. A
  breaking change inside a major version is not allowed.
- Before a surface reaches 1.0, a breaking change replaces the old shape
  instead of shipping beside it.
- Each major-version step must publish a per-surface compatibility envelope and a
  migration guide.

### Confirmation

- Contract compatibility tests against the prior major version, run as a gate on
  every release: compile-time tests for the SDK trait and the Plugin SPI, and
  schema-diff tests for the REST API. Each gate starts at the 1.0 release of its
  surface, since before then there is no prior major to compare against.
- Review that each surface records its own version trajectory and stability
  state.
- Release-process review, to show that deployment tooling supports two concurrent
  major versions of one surface.

## Pros and Cons of the Options

### Independent major-version stability per surface

Each surface versions on its own track. Changes inside a major version are
additive, and one prior major stays supported.

- Good, because a plugin author and an in-process consumer each migrate on a
  schedule that fits their own release cadence.
- Good, because a breaking change on one surface forces no breaking change on the
  others, so churn stays low.
- Good, because each surface's compatibility envelope is explicit and
  machine-checkable through contract tests.
- Neutral, because the support matrix holds at most two concurrent majors per
  surface, which stays tractable.
- Bad, because a refactor that crosses several surfaces needs per-surface
  staging. That costs more than one shared-version refactor.

### Single shared version across surfaces

All three surfaces share one major-version number that advances together.

- Good, because the release artifact is one coherent version, with unified
  documentation and one migration guide.
- Bad, because a breaking change on one surface forces every other surface into a
  new major, even when its contract does not change.
- Bad, because a plugin author and a remote caller both follow the release cycle
  of whichever surface churns most.
- Bad, because every ecosystem participant must migrate at the same time, rather
  than on its own track.

### Calendar-versioned release train

The surfaces ship together on a fixed schedule, for example quarterly, with an
additive-or-breaking label per release.

- Good, because the cadence is predictable and the planning horizon is fixed.
- Bad, because the surfaces still travel together, which imports the coupling of
  the shared-version option.
- Bad, because a calendar cadence does not match the natural cadence of a
  plugin-author or remote-caller ecosystem.
- Bad, because a breaking change then arrives on a calendar date rather than when
  the ecosystem is ready.

## More Information

Related decisions:

- `cpt-cf-usage-collector-adr-pluggable-storage` — the Plugin SPI whose stability
  this decision governs.
- `cpt-cf-usage-collector-adr-mandatory-idempotency` — the mandatory contract
  field whose later relaxation this decision governs.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-nfr-plugin-contract-stability` — major-version
  stability on each public surface.
- `cpt-cf-usage-collector-principle-contract-stability` — the design principle
  that this decision codifies.
- `cpt-cf-usage-collector-constraint-plugin-contract-stability` — the constraint
  that pairs with this decision.
- `cpt-cf-usage-collector-interface-rest-api`,
  `cpt-cf-usage-collector-interface-sdk-client`, and
  `cpt-cf-usage-collector-interface-plugin` — the three surfaces that this
  decision governs.
