---
status: accepted
date: 2026-04-20
---
<!--
Created:  2026-04-20 by Constructor Tech
Updated:  2026-04-20 by Constructor Tech
-->

<!--
=============================================================================
ARCHITECTURE DECISION RECORD (ADR) — based on MADR format
=============================================================================
PURPOSE: Capture WHY runtime-tier concerns (invocation engine, scheduler,
event trigger engine, retries, checkpointing) are placed in runtime plugins
rather than the host gear, and why no host-owned runtime-neutral durability
substrate is provided to plugins.

RULES:
- ADRs represent actual decision dilemma and decision state
- DESIGN is the primary artifact ("what"); ADRs annotate DESIGN with rationale ("why")
- Use single ADR per decision

STANDARDS ALIGNMENT:
- MADR (Markdown Any Decision Records)
- IEEE 42010 (architecture decisions as first-class elements)
- ISO/IEC 15288 / 12207 (decision analysis process)
==============================================================================
-->
# ADR — Serverless Runtime: Thin Host Gear, Fat Runtime Plugins


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Option C: Thin host, fat runtime plugin — **chosen**](#option-c-thin-host-fat-runtime-plugin--chosen)
  - [Option A: Thin executor, fat host (pre-1279 DESIGN.md §3.2) — rejected](#option-a-thin-executor-fat-host-pre-1279-designmd-32--rejected)
  - [Option B: Three-tier orchestrator + stateless runtime + thin adapter with host callbacks (PR 1279) — rejected](#option-b-three-tier-orchestrator--stateless-runtime--thin-adapter-with-host-callbacks-pr-1279--rejected)
- [More Information](#more-information)
  - [Decision Review Triggers](#decision-review-triggers)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-serverless-runtime-adr-thin-host`

## Context and Problem Statement

The serverless-runtime gear has two legitimate places to put execution-tier concerns — invocation lifecycle, scheduling, event-triggered dispatch, durable timers, checkpoint persistence, and retry orchestration. The recent DESIGN.md rewrite (PR [#1279](https://github.com/constructorfabric/gears-rust/pull/1279), commit [`1a5ae2b7`](https://github.com/constructorfabric/gears-rust/commit/1a5ae2b7), merged as [`efdef7d0`](https://github.com/constructorfabric/gears-rust/commit/efdef7d0)) places these in a stateful host gear (`sless-orchestrator`) and exposes them to runtime plugins through an `ExecutionContext` callback surface, with plugins declaring an `autonomous` capability to bypass that surface when their backend brings its own durability.

This ADR revisits that boundary and asks: for the set of runtime backends this platform is actually committed to — or will plausibly add — does host-owned orchestration pay for itself, or is its shape driven by one backend class at a cost to every other backend?

## Decision Drivers

* The platform must support pluggable execution runtimes without forcing each runtime to fake primitives its technology already provides natively
* Design complexity (additional module, additional traits), operational complexity (Job Worker with distributed advisory locking, checkpoint store, timer wheel, event-matching engine), and cross-gear boundary complexity (`JobTransport`, `ExecutionContext`) must pay for themselves against concrete, non-speculative requirements
* KISS and YAGNI are first-class project development principles — shaping the host boundary around one backend class at the expense of every other backend violates them
* The gear boundary must not be tailored to the needs of any single runtime backend; backend-specific requirements should be handled inside that backend's plugin, not by reshaping the host for all backends
* Adapter conformance test suites (already called for by DESIGN `cpt-cf-serverless-runtime-nfr-resource-governance`) can enforce uniform user-visible semantics regardless of where runtime primitives are implemented, so uniform semantics do not by themselves require host-owned durability
* Tenant-level governance (quotas, retention, runtime allowlist) is orthogonal to execution technology and must remain centralised in the host
* Runtime plugins must be mutually isolated at the crate dependency level — no plugin may depend on another plugin's crate
* Aggregate queries across tenants (for example "list all invocations for tenant X") must be answerable without fan-out proportional to the number of registered adapter plugins
* The platform already has an ADR about picking Temporal as the reference workflow engine — which is a concrete data point about how the first real backend behaves, but the boundary question in this ADR does not depend on any specific runtime choice

## Considered Options

* **Option A**: Thin executor, fat host — pre-1279 DESIGN.md §3.2 boundary; host owns Invocation Engine, Scheduler, Event Trigger Engine; adapters only execute code
* **Option B**: Three-tier orchestrator + stateless runtime + thin adapter with host-owned durability callbacks — current `main` after PR 1279; runtime-neutral durability primitives in host, bypassable by adapters that self-declare `autonomous`
* **Option C**: Thin host, fat runtime plugin — host owns only genuinely cross-cutting concerns (Registry, Tenant Policy, REST façade, GTS validation, audit, plugin dispatch); each plugin owns Invocation Engine, Scheduler, Event Trigger Engine using its backend's native primitives; in-process runtimes that need durability later consume a shared Rust helper crate rather than host infrastructure

## Decision Outcome

Chosen option: **"Option C: Thin host, fat runtime plugin"**, because the backends Option B's host-owned primitives actually serve are in-process script runners (such as the planned Starlark backend). A single backend class's lack of native durability does not justify tailoring the host boundary around it at the expense of every other backend — which would then either route through a substrate they do not need or opt out via `autonomous` (the escape hatch that already undermines the uniformity argument). The backend-durability survey below shows this asymmetry directly: every backend with external durability brings its own primitives; only in-process runners lack them. Paying the design and operational cost of a runtime-neutral orchestration substrate so that one backend class can reuse it — while every other backend pays the tax and bypasses the surface — violates the project's KISS and YAGNI principles.

### Consequences

* The serverless-runtime area collapses back to **one module**: `gears/serverless-runtime/` with `serverless-runtime-sdk/` (contract crate) and `serverless-runtime/` (host implementation crate). The `sless-runtime` gear introduced by PR 1279 is not retained as a separate gear.
* Runtime plugins live at `gears/serverless-runtime/plugins/<backend>-plugin/` as standalone crates. Host crate **must not** depend on any plugin crate at compile time; plugins are resolved at runtime through ClientHub scoped registration keyed by GTS adapter type.
* The `RuntimeAdapter` trait, declared in `serverless-runtime-sdk`, becomes the primary plugin contract and includes the invocation, control, schedule, and event-trigger methods. Each plugin implements the trait; the host dispatches to the plugin through `dyn RuntimeAdapter`. The plugin emits index updates back to the host through a thin event port on the host client (`ServerlessRuntimeClient`, exported from `serverless-runtime-sdk`), not through a general-purpose callback surface.
* `JobTransport` and `ExecutionContext` abstractions from PR 1279 will be removed from the SDK. The `AdapterCapabilities::autonomous` flag will no longer be meaningful because every plugin is autonomous by design.
* The host does not ship a `Job Worker`, checkpoint store, timer wheel, or event-matching engine. Each plugin implements these using its backend's native primitives (Temporal's Schedule API and signals, EventBridge Scheduler and SQS for Lambda, Azure Durable's native timers, etc.).
* Invocation records use a **host-indexed, plugin-detailed** split: the host persists a lightweight, queryable index (id, function_id, adapter, tenant, owner, status, timestamps, error summary) populated from plugin-emitted events. The plugin owns the full `InvocationRecord`, the timeline, and any internal execution state. Aggregate queries and tenant-wide listings read the host index; deep fetches (timeline, payloads) delegate to the plugin.
* In-process runtimes that lack native durability (such as the planned Starlark backend, or a potential WASM backend) consume a shared Rust helper crate that provides those primitives at the plugin level. This scopes the complexity to the plugins that need it, rather than pushing it into the host for every plugin to route through.
* Adapter conformance test suites in `serverless-runtime-sdk` become load-bearing for ensuring uniform user-visible semantics across plugins (status transitions, retry contract, compensation triggering, suspension visibility).
* DESIGN.md sections written by PR 1279 that depend on Option B (`§1.4.1`–`§1.4.5`: gear split, `JobTransport`, `ExecutionContext`, capability-based dispatch) will be rewritten in a follow-up PR to reflect this decision. That follow-up PR is out of scope of this ADR.

### Confirmation

Acceptance criteria to apply once the host and plugin crates are in place:

* Ensure the follow-up DESIGN.md rewrite enumerates host-owned components as: Function Registry, Tenant Policy, REST façade, GTS validation, audit, plugin dispatch, and the lightweight invocation index — and that `sless-runtime` is not reintroduced as a separate gear, and `JobTransport`, `ExecutionContext`, and host-owned durability primitives are not reintroduced
* Ensure the `RuntimeAdapter` trait, declared in `serverless-runtime-sdk`, includes invocation, control, schedule, and event-trigger methods, and that `JobTransport` and `ExecutionContext` types are not present in the SDK
* Ensure no host crate (`serverless-runtime/serverless-runtime`) depends on any plugin crate — verify with `cargo tree`
* Ensure the host crate source tree contains no `job_worker`, `timer_wheel`, `checkpoint_store`, or `event_matcher` gears — verify via directory listing
* Add adapter conformance tests in `serverless-runtime-sdk` covering: invocation status transitions, retry semantics, compensation triggering, suspension/resume visibility, error taxonomy
* During code review, confirm that each plugin crate implements scheduling and event-trigger handling inside the plugin itself and does not call into host durability APIs

## Pros and Cons of the Options

### Option C: Thin host, fat runtime plugin — **chosen**

The host owns only genuinely cross-cutting concerns. Each runtime plugin is a self-contained adapter that implements invocation, scheduling, and event-triggered dispatch using the native primitives of its underlying technology.

**Gear shape:**

```
gears/serverless-runtime/
├── serverless-runtime-sdk/        # Contract crate — RuntimeAdapter, ServerlessRuntimeClient,
│                                  # domain types, error taxonomy, conformance harness hooks
├── serverless-runtime/            # Host impl — Registry, Tenant Policy, REST, GTS validation,
│                                  # audit, plugin dispatch
└── plugins/
    ├── temporal-plugin/           # First adapter — uses Temporal for durability
    ├── lambda-plugin/             # (future) — uses Step Functions + EventBridge
    └── …                          # Further backends, one plugin each
```

**Backend-durability survey**: for each committed or realistic backend, does the backend need host-owned durability primitives, or does it bring them itself?

| Backend | Durable timers | Signals/events | Retry | Checkpointing | Scheduling | Needs host-owned durability? |
|---|---|---|---|---|---|---|
| Temporal (subject of an accepted ADR about picking the first workflow engine) | Native | Native signals | Native retry policy | Native workflow history | Native Schedule API | **No** |
| AWS Lambda + Step Functions | Step Functions `Wait` | EventBridge + SQS | SQS retry + DLQ | Step Functions state | EventBridge Scheduler | **No** |
| Azure Durable Functions | Native timers | Native events | Native | Native | Native | **No** |
| Google Cloud Run Jobs + Workflows | Cloud Workflows | Eventarc | Built-in | Workflows state | Cloud Scheduler | **No** |
| In-process script runner (Starlark — planned future backend; WASM — potential) | None | None | None | None | None | Yes |

Every backend with external durability brings its own primitives. The backends that would consume host-owned durability are in-process script runners — a single class that lacks native primitives because of what the backend technology is, not because the platform's production workloads require a runtime-neutral orchestration substrate. Tailoring the host boundary to serve that single class creates "tail wagging the dog" coupling across every other backend, which must pay the substrate cost or opt out via `autonomous`.

| | Aspect | Note |
|---|---|---|
| Pro | Each backend uses the primitives its technology is built around | No translation layer between user-visible semantics and backend-native mechanics; no two competing state machines |
| Pro | No host-level durable-execution engine to build, operate, or monitor | No Job Worker, no distributed advisory lock, no checkpoint store, no timer wheel, no event-matching engine in the host |
| Pro | Plugin crates are mutually isolated | Each plugin depends only on the SDK contract crate; host does not depend on any plugin |
| Pro | Extensibility cost scales with actual need | In-process backends that arrive later get a shared helper crate scoped to plugins that opt in; host stays thin |
| Pro | KISS / YAGNI compliance | Complexity is paid only for backends that exist |
| Neutral | Cross-plugin listing requires host-indexed invocation projections | Host maintains a lightweight event-fed index for aggregate queries; plugins own full detail. One more indirection than fully plugin-owned records, but strictly less than the full durability engine in Option B |
| Neutral | Uniform user-visible semantics depend on conformance tests, not shared implementation | Adapter conformance suite becomes load-bearing; already called for by `cpt-cf-serverless-runtime-nfr-resource-governance` |
| Con | No single codebase enforces runtime-neutral orchestration | Each plugin author must re-follow the conformance contract; review burden is slightly higher per plugin |

### Option A: Thin executor, fat host (pre-1279 DESIGN.md §3.2) — rejected

The host gear owns Invocation Engine, Scheduler, and Event Trigger Engine. Adapters are pure executors: "given a validated function, run it." This was the boundary before PR 1279's rewrite.

| | Aspect | Note |
|---|---|---|
| Pro | Simple mental model | One orchestration engine, many dumb executors |
| Pro | Uniform retry / scheduling semantics by construction | Shared code path |
| Con | Ignores that most realistic backends already have invocation engines | Temporal, Step Functions, Azure Durable all orchestrate themselves — forcing them to run through a neutral host engine creates two competing state machines |
| Con | Host must own checkpoint storage, timer wheels, event matching | Duplicates primitives that backends already implement |
| Con | No escape hatch for autonomous backends | Rejected because it loses the one strength Option B added |

### Option B: Three-tier orchestrator + stateless runtime + thin adapter with host callbacks (PR 1279) — rejected

A stateful `sless-orchestrator` gear owns Registry, Invocation Engine, Scheduler, Event Trigger Engine, Job Worker, Tenant Policy, and persistence. A separate stateless `sless-runtime` gear hosts runtime adapter plugins. Plugins implement a thin `RuntimeAdapter` trait (`execute`, `cancel`, `handle_control_action`, `capabilities`) and interact with the host through an `ExecutionContext` callback surface (`invoke`, `checkpoint`, `wait_for_event`, `sleep`, `report_event`). Plugins whose backends bring their own durability (Temporal) self-declare `AdapterCapabilities::autonomous` and bypass the callback surface.

| | Aspect | Note |
|---|---|---|
| Pro | Uniform user-visible primitives across all adapters that use the callback surface | One source of truth for durable behaviour in the non-autonomous path |
| Pro | Cross-plugin aggregate queries work naturally | Orchestrator owns all state, so listing is a single DB query |
| Neutral | Clean separation between orchestration and execution | Conceptually elegant, but the cost is real |
| Con | Host boundary is shaped by the needs of one backend class | Every other backend — Temporal, Lambda, Azure Durable, Cloud Run — either routes through a substrate they do not need or opts out via `autonomous`. Letting in-process script runners determine the architecture for all backends is the pattern KISS / YAGNI is meant to prevent |
| Con | Every backend with external durability opts out | The design admits this — `DESIGN.md:875` states autonomous adapters may bypass `ExecutionContext`. The callback surface is infrastructure whose caller set is the in-process backend class, paid for by all backends |
| Con | Host operates a durable-execution engine in its own right | Job Worker with distributed advisory lock, checkpoint store, timer wheel, event-matching engine. Each is a real operational concern (DB contention, failure mode, monitoring surface) paid for by every backend, on behalf of one backend class |
| Con | Two-gear split (`sless-orchestrator` + `sless-runtime`) | More cross-crate boundaries and another `JobTransport` abstraction to maintain, justified only by the callback surface the other backends bypass |
| Con | `autonomous` capability is an escape hatch that weakens the uniformity argument | If autonomous adapters can diverge on suspension, signalling, and checkpoint semantics, the host surface does not by itself guarantee uniformity — conformance tests do, and those would still be needed under Option C |
| Con | Violates KISS / YAGNI | One backend class's shape drives host-level infrastructure every other backend pays for but does not need. "Tail wagging the dog", not "no demand exists" |

## More Information

**PR 1279 reference**:
- Pull request: https://github.com/constructorfabric/gears-rust/pull/1279
- Source commit: [`1a5ae2b7`](https://github.com/constructorfabric/gears-rust/commit/1a5ae2b7) — `docs(serverless-runtime): GTS renames, sibling type hierarchy, schema fixes`
- Merge commit: [`efdef7d0`](https://github.com/constructorfabric/gears-rust/commit/efdef7d0)

**Evidence that Option B expects its primary backend to bypass its own callback surface**: current DESIGN.md (post-merge of PR 1279) states in its discussion of adapter behaviour by type that autonomous adapters such as Temporal may bypass `ExecutionContext` in favour of their own infrastructure. Option C takes that observation as the premise for a simpler boundary: if the adapter is autonomous anyway, no callback surface needs to exist.

**Project development principles**: this project treats KISS and YAGNI as first-class engineering principles. Option B builds a runtime-neutral durability substrate whose shape is determined by the needs of one backend class (in-process script runners), but whose cost is paid by every backend — either through routing through infrastructure they do not need, or through the `autonomous` escape hatch that already undermines the uniformity argument the substrate was meant to provide. This is the form of coupling-at-a-distance those principles are intended to prevent.

**Relationship to adjacent ADRs**: the choice of a concrete workflow engine for the platform is documented in a separate ADR. The platform's plans already mix backends with native durability (Temporal) and backends without (the planned Starlark backend). That mix is the typical case, not an edge case, and it is precisely the reason the host boundary must not be tailored to either class. Plugins that have their own primitives use them; plugins that need primitives consume a scoped helper crate. The host stays uniform across both. This ADR does not depend on any single engine choice.

**Standards alignment**:
- MADR (Markdown Any Decision Records)
- IEEE 42010 — architecture decisions as first-class elements
- ISO/IEC 15288 / 12207 — decision analysis process

### Decision Review Triggers

This decision should be revisited if any of the following change:
- A plugin-level shared durability helper proves inadequate at scale across multiple in-process plugins, and a single host-owned substrate becomes the only practical way to avoid duplicated primitives
- A future backend is chosen that is not autonomous and cannot efficiently serve its durability needs from its own native primitives or a scoped helper crate
- Adapter conformance test coverage proves inadequate for keeping user-visible semantics uniform across plugins, and a shared host implementation becomes the only practical way to restore uniformity

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following design elements:

* `cpt-cf-serverless-runtime-principle-pluggable-adapters` — pluggable adapter principle; this ADR refines its boundary so plugins are fat and self-contained rather than thin executors routed through a host durability substrate
* `cpt-cf-serverless-runtime-nfr-resource-governance` — Pluggability NFR; this ADR makes adapter conformance tests the primary uniform-semantics mechanism, replacing host-owned runtime neutrality
* `cpt-cf-serverless-runtime-component-executor` — Executor component scope; this ADR reassigns Invocation Engine, Scheduler, and Event Trigger Engine responsibilities from host into plugins
