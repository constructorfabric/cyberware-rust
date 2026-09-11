---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team (Architecture)
---

# ADR-0001: Orchestrate Over Gear-Owned Durable State, Not Engine History


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [An external durable-execution engine as the substrate](#an-external-durable-execution-engine-as-the-substrate)
  - [Orchestration over gear-owned durable state (chosen)](#orchestration-over-gear-owned-durable-state-chosen)
  - [A hybrid with dual-written engine and gear-owned history](#a-hybrid-with-dual-written-engine-and-gear-owned-history)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-workflow-adr-durable-execution-substrate`
## Context and Problem Statement

Orders Workflow is a long-running process orchestrator: approval waits, provisioning intents, retries and durable timers must survive restarts with zero loss for committed steps. PRD §13 defers the durable-execution platform choice to this ADR, naming the OSS Workflow Engine (`PRD-workflow-engine-202501051430`) and a BSS-local mechanism as candidates. PRD §6.1 fixes a constraint ahead of that choice: gear-owned process audit and saga log — independent of whatever execution substrate is used — is the audit system of record (SoR) for process-execution progress, never engine execution history. Which substrate satisfies that constraint without a second, competing audit trail?

## Decision Drivers

* Gear-owned process audit and saga log must be the audit SoR (§6.1), independently of engine history, which is explicitly not the audit SoR.
* Zero data loss for committed steps (§7.1) must hold regardless of engine purge or retention policy.
* Commercial data appearing inside execution history (resolved total in approval context, approver identities, tenant axes, saga log) needs isolation and retention this gear controls, not a shared engine's default window.
* The BSS/OSS boundary (design principle carried from sibling gears) keeps a BSS process gear from depending on an OSS-owned execution store for its compliance-grade record.
* A process instance must execute to completion under the definition version it started with (§6.1); the substrate must not silently migrate a running instance.
* Two sibling gears (`orders-lifecycle`) already solved an analogous "one component owns the guaranteed effect" problem (ADR-0001 there); divergence from that shape costs review effort without a compensating benefit here.

## Considered Options

* An external durable-execution engine (e.g., the OSS Workflow Engine) as the substrate, with its execution history treated as the operational record
* Orchestration over gear-owned durable state: this gear persists step progress, retry counters, the saga/compensation log, timer handles and approval-request tracking itself, and treats any execution engine underneath (if one is used at all) as a mechanical replay/scheduling aid whose history is disposable
* A hybrid where the external engine's history is dual-written into a gear-owned audit table

## Decision Outcome

Chosen option: **orchestration over gear-owned durable state**, because it is the only option under which "engine history is not the audit SoR" (§6.1) is a structural property rather than an operational promise. The gear owns its process-audit and saga-log tables directly; step progress, retry counters, timer handles, approval-request tracking, the process `correlationId` and the process-definition version are written there in the same durability path the gear controls end to end. If an execution engine or scheduler is used underneath for retry/timer mechanics, its internal history is treated as disposable scaffolding, never as a citable record, and nothing downstream (audit exports, replay, operator tooling) may read from it.

### Consequences

* This gear must implement its own durable-state model: an append-capable process-audit log, a saga/compensation log, and durable timer/retry bookkeeping, rather than delegating those to an external engine's guarantees.
* Any execution engine adopted later for scheduling or retry mechanics is replaceable without an audit migration, because the audit SoR never lived there.
* Replay and recovery must be defined against the gear-owned record (per §7.1's zero-loss requirement), so the design must specify how a crashed or restarted instance reconstructs its position from that record alone.
* Retention and isolation of process artifacts (§6.1's data-classification note) become this gear's responsibility rather than inherited from an engine's default window, which increases the gear's own storage and lifecycle-management surface.
* Future extensibility for a different scheduling mechanism is possible only insofar as it can be plugged in below the audit boundary without becoming a second writer of process-execution state.
* **Substrate selection remains open.** This ADR fixes where the audit source of record lives; it selects no durable-execution engine, mechanism or product. The selection is a separate, still-unresolved decision tracked as `DECISIONS.md` Q-01 (owner: Architecture), and no document may cite this ADR as having made it. Until Q-01 closes, no slice's saga-step, durable-timer or engine-history-isolation implementation can be finalized.

### Confirmation

Confirmed by a structural test asserting no code path in this gear treats engine-side execution history as a read source for audit, replay, or operator-facing progress; and a fault-injection/restart test asserting an in-flight process reconstructs step progress, retry counters and timer state solely from the gear-owned record after a forced restart, with zero loss for already-committed steps.

## Pros and Cons of the Options

### An external durable-execution engine as the substrate

An off-the-shelf workflow engine (e.g., the OSS Workflow Engine) provides durability, retries and timers as platform features, with its execution history available for inspection.

* Good, because durability, retry and timer mechanics are provided rather than built.
* Good, because it matches a widely understood workflow-engine model.
* Neutral, because the engine may still be used underneath this decision purely for scheduling, provided its history is not cited as SoR.
* Bad, because engine execution history commonly carries commercial context (resolved totals, approver identities) under a retention/purge window this gear does not own, which directly conflicts with §6.1's independence requirement.
* Bad, because it blurs the BSS/OSS boundary: a BSS compliance-grade audit trail would depend on an OSS-owned store's retention policy.

### Orchestration over gear-owned durable state (chosen)

The gear persists its own process-execution state and treats any underlying execution engine as replaceable scheduling infrastructure.

* Good, because the audit SoR is gear-owned by construction, satisfying §6.1 without relying on operational discipline.
* Good, because retention and isolation of process artifacts are set by this gear's own policy, matching the tenant-scoping and audit-grade requirements in §6.1.
* Good, because the process-definition-version pinning requirement (no mid-flight migration) is enforceable against a record this gear controls.
* Neutral, because an external engine may still be used underneath for mechanical scheduling without changing this decision.
* Bad, because this gear must build and operate its own durable-state and replay machinery instead of inheriting it from a platform component.

### A hybrid with dual-written engine and gear-owned history

The external engine's execution history is copied into a gear-owned audit table after the fact.

* Good, because it lets teams start from an engine's built-in durability while working toward gear ownership.
* Bad, because two writers of "process history" create a reconciliation problem exactly analogous to the dual-SoR drift §6.1 exists to prevent, only one level down.
* Bad, because a lag or failure in the copy step reintroduces the zero-loss risk this ADR exists to close.

## More Information

This decision is scoped to the durable-execution substrate only; it does not select or exclude any specific engine product for scheduling mechanics underneath the gear-owned audit boundary. That product choice, if any, remains an implementation detail confirmed by the test in Confirmation above, and it is **open** — it is tracked as `DECISIONS.md` Q-01 and is not made anywhere in this design set. A statement elsewhere that this ADR "selects the durable-execution engine/mechanism" is a misreading of it. See the sibling `orders-lifecycle` gear's ADR-0001 (`transition-through-engine`) for the analogous reasoning pattern of making a `p1` guarantee structural rather than a discipline, applied there to state transitions rather than process-execution durability.

## Traceability

- **PRD**: [PRD.md](../PRD.md) — §6.1, §7.1, §13, §15, §16
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-workflow-fr-owf-process-state-nonauth` — this decision is what makes gear-owned process audit the SoR independently of engine history, rather than a rule restated per handler
* `cpt-cf-bss-orders-workflow-nfr-owf-durability` — zero-loss durability for committed process state is asserted against the gear-owned record this decision establishes
* `cpt-cf-bss-orders-workflow-nfr-owf-audit` — 100% process-state-transition audit coverage is achievable only if the audit path is gear-owned end to end
* `cpt-cf-bss-orders-workflow-nfr-owf-retention` — retention of process artifacts is set by this gear's own policy rather than an inherited engine window
