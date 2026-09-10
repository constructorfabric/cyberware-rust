---
status: accepted
date: 2026-03-30
---
<!--
Created:  2026-03-30 by Constructor Tech
Updated:  2026-04-22 by Constructor Tech
-->

<!--
=============================================================================
ARCHITECTURE DECISION RECORD (ADR) — based on MADR format
=============================================================================
PURPOSE: Capture WHY Temporal was chosen as the durable execution backend
and WHY a custom workflow engine layer interprets the DSL on top of it.

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
# ADR — Temporal-based Workflow Engine


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Option A: Temporal](#option-a-temporal)
  - [Option B: Cadence](#option-b-cadence)
  - [Option C: Camunda / Zeebe](#option-c-camunda--zeebe)
  - [Option D: Restate](#option-d-restate)
  - [Option E: AWS Step Functions](#option-e-aws-step-functions)
  - [Option F: Apache Airflow](#option-f-apache-airflow)
- [More Information](#more-information)
  - [Decision Review Triggers](#decision-review-triggers)
  - [Security Note](#security-note)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-serverless-runtime-adr-temporal-workflow-engine`
## Context and Problem Statement

The Serverless Runtime domain model defines a pluggable runtime architecture (`cpt-cf-serverless-runtime-principle-pluggable-adapters` — the traceability ID retains the legacy `adapters` slug; the design now consistently uses "runtime plugin" for the gear and `RuntimeAdapter` for the trait it implements). This ADR covers the **Temporal runtime plugin** — one of multiple possible runtime plugin implementations (others include Starlark, WASM, cloud FaaS). The Temporal runtime plugin needs a workflow engine that:

1. **Interprets workflow definitions** authored in the Serverless Workflow Specification DSL ([ADR-0003](0003-cpt-cf-serverless-runtime-adr-workflow-dsl.md)) — parsing task definitions, evaluating expressions, managing data flow between steps, and controlling execution flow (branching, loops, error handling).
2. **Executes workflows durably** — persisting state across service restarts, providing automatic checkpointing, retry, compensation, and long-running workflow support lasting days to months.


## Decision Drivers

* The engine must support durable execution with automatic checkpointing, suspend/resume across service restarts, and continuation from the last completed step (BR-003, BR-009)
* The engine must support long-running workflows lasting days to months, including suspension periods of at least 30 days with event-driven continuation (BR-009)
* The engine must support durable execution primitives sufficient for saga-style compensation (BR-133)
* The engine must support idempotency mechanisms including idempotency keys and deduplication windows (BR-134)
* Multi-tenant isolation is mandatory — one tenant's workflows must not affect another tenant's execution, state, or data (BR-002, BR-036, BR-206)
* The engine must have a Rust SDK for integration without FFI bridges or sidecar processes (project constraint — the platform is built in Rust)
* The engine must be self-hostable with no vendor lock-in — the platform must control its own infrastructure (BR-035)
* The engine must integrate as a standard ToolKit plugin gear, following the established plugin architecture pattern (DESIGN: `cpt-cf-serverless-runtime-principle-pluggable-adapters`)
* The engine must implement the `RuntimeAdapter` trait interface (DESIGN: `cpt-cf-serverless-runtime-principle-pluggable-adapters`)
* The engine must support configurable retry policies with exponential backoff and non-retryable error classification (BR-019)
* The engine must provide execution visibility — queryable execution status, history, and timeline events (BR-015, BR-130)
* Schedule-based triggers with cron/interval expressions and missed schedule handling must be supported (BR-022, BR-110)
* The engine must meet reliability targets: availability >=99.95%, RTO <=30s, RPO <=1min (BR-026)
* The engine must meet performance targets: start latency p95 <=100ms, step dispatch p95 <=50ms (BR-207)
* The engine must scale to >=10K concurrent executions, >=1K starts/sec, >=1K tenants (BR-208)

## Considered Options

* **Option A**: Temporal (open-source durable execution platform)
* **Option B**: Cadence (Uber's original workflow engine, predecessor to Temporal)
* **Option C**: Camunda / Zeebe (BPMN-based workflow automation)
* **Option D**: Restate (event-driven durable execution)
* **Option E**: AWS Step Functions (cloud-native state machine)
* **Option F**: Apache Airflow (DAG-based workflow orchestration)

## Decision Outcome

Chosen option: **"Option A: Temporal"** as the durable execution backend, with a **custom workflow engine layer** built on top using the Temporal Rust SDK.

**Why Temporal**: It is the only option that satisfies all decision drivers — particularly the combination of self-hostable deployment, Rust SDK availability (`temporal-sdk-core` is written in Rust and usable from Rust; standalone `temporalio-sdk` crate is prerelease as of 2026-03-30 — see [sdk-core](https://github.com/temporalio/sdk-core)), proven scale at production users, long-running workflow support (days to years), saga/compensation patterns, and built-in schedule API.

**Why a custom engine layer on top**: Temporal provides durable execution primitives (checkpointing, retry, state persistence, worker management) but does not natively interpret the Serverless Workflow Specification DSL ([ADR-0003](0003-cpt-cf-serverless-runtime-adr-workflow-dsl.md)). A custom engine layer is required to bridge the gap between declarative workflow definitions and Temporal's workflow-as-code model — handling DSL parsing, task execution mapping, expression evaluation, flow control, and data transformation.

### Consequences

* The workflow engine is implemented as a Temporal worker service that maps DSL task types to Temporal workflows and activities.
* The engine architecture has two layers: the Temporal SDK layer (durable execution, state persistence, retry, scheduling) and a custom workflow engine layer on top (DSL parsing, task execution, expression evaluation, flow control).
* A Temporal Server deployment becomes an infrastructure dependency. The engine connects to Temporal Server via the Temporal Rust SDK (`temporal-sdk-core`). Temporal Server itself requires a persistence backend (PostgreSQL, MySQL, or Cassandra).
* Multi-tenant isolation is achieved at the application layer through `tenant_id`/`user_id` scoping in database queries and ownership checks.
* Compensation is implemented at the DSL level through Try/Raise task composition (see [ADR-0003](0003-cpt-cf-serverless-runtime-adr-workflow-dsl.md)); the engine provides the durable execution guarantees that make this reliable.
* Schedule management maps to Temporal's built-in Schedule API.
* The engine integrates as a standard ToolKit plugin gear.

### Confirmation

* The engine correctly parses, validates, and executes workflows authored in Serverless Workflow Specification YAML through the full lifecycle: submission → validation → Temporal dispatch → task execution → completion
* Integration tests verify multi-tenant isolation: workflow created in tenant A is not visible or accessible from tenant B
* Schedule-based and event-driven triggers create and execute workflows correctly
* Performance benchmarks validate start latency p95 <=100ms and step dispatch p95 <=50ms under test load
* Plugin registration works correctly via ToolKit plugin pattern

## Pros and Cons of the Options

### Option A: Temporal

Temporal is an open-source durable execution platform that provides workflow orchestration with automatic state persistence, built-in retry/timeout handling, and native support for long-running processes.

| | Aspect | Note |
|---|--------|------|
| Good | Rust SDK available (`temporal-sdk-core`) | Written in Rust and usable from Rust; primarily serves as the polyglot core (other language SDKs use it via FFI); standalone `temporalio-sdk` crate is prerelease (as of 2026-03-30 — see [sdk-core](https://github.com/temporalio/sdk-core)) |
| Good | Built-in durable execution | Automatic checkpointing, event sourcing, and deterministic replay — workflow state survives infrastructure failures without custom persistence logic |
| Good | Proven at scale | Production deployments handling high workflow volumes at multiple large-scale users |
| Good | Long-running workflow support | Workflows can run for days to years with timer-based and event-driven continuation; suspension periods of 30+ days are natively supported |
| Good | Durable execution for compensation | Temporal's checkpointing and retry guarantees make DSL-level compensation (Try/Raise) reliable across failures |
| Good | Built-in schedule API | Cron and interval schedules with configurable missed schedule policies, reducing custom schedule infrastructure |
| Good | Self-hostable, MIT license | No vendor lock-in; full control over infrastructure and data |
| Good | Workflow visibility API | Built-in query interface for execution status, history, and search — maps to `InvocationRecord` and timeline events |
| Neutral | Multi-tenancy | Tenant isolation handled at application/DB layer; Temporal namespaces available for additional logical separation if needed |
| Good | Active community and ecosystem | Regular releases, SDKs in multiple languages, Slack community, conference presence |
| Neutral | Temporal Server infrastructure | Requires deploying and operating Temporal Server (with its own persistence), but only when the plugin is enabled |
| Neutral | Worker-based execution model | Per-execution resource limits (memory, CPU) are not enforced at the runtime plugin level — relies on worker pool sizing and Temporal Server rate limiting |
| Bad | Operational complexity | Temporal Server adds infrastructure overhead (server cluster, persistence backend, monitoring) |
| Bad | Deterministic execution constraints | Engine implementation code must follow Temporal's deterministic replay rules (no side effects in workflow code); this affects engine developers, not workflow authors who write declarative YAML |
| Bad | Custom engine layer required | Temporal does not natively interpret the Serverless Workflow Specification; a custom DSL interpreter layer adds development and maintenance complexity |
| Bad | Multi-process architecture | Temporal Server runs as a separate process; all workflow ↔ activity communication requires gRPC serialization/deserialization and IPC, adding latency overhead compared to in-process execution engines |
| Bad | Rust SDK maturity | `temporal-sdk-core` is primarily the polyglot foundation; standalone Rust SDK (`temporalio-sdk`) is prerelease (as of 2026-03-30 — see [sdk-core](https://github.com/temporalio/sdk-core)) — API stability risk |

### Option B: Cadence

Cadence is the predecessor to Temporal, originally developed at Uber. It provides similar durable workflow capabilities but development has slowed since the Temporal fork.

| | Aspect | Note |
|---|--------|------|
| Good | Proven at Uber scale | Production-tested at massive scale |
| Good | Similar programming model to Temporal | Workflow-as-code with automatic checkpointing |
| Good | Self-hostable | Open-source, no vendor lock-in |
| Bad | No production Rust SDK | Only Go and Java SDKs are production-ready; Rust support would require building from scratch |
| Bad | Reduced community momentum | Slower release cadence and fewer ecosystem integrations since the Temporal fork |
| Bad | Feature parity lag | Temporal has added features (advanced visibility, schedule API, Nexus for cross-namespace calls) that Cadence lacks |

### Option C: Camunda / Zeebe

Camunda provides BPMN-based workflow orchestration. Camunda 8 (Zeebe) offers a cloud-native, horizontally scalable engine.

| | Aspect | Note |
|---|--------|------|
| Neutral | Strong enterprise governance | Built-in audit trails, user task management, and decision tables |
| Good | Horizontal scalability | Zeebe's partitioned architecture scales to high throughput |
| Bad | No Rust SDK | Java/C# SDKs only (Go SDK deprecated); Rust integration requires gRPC client implementation from scratch |
| Bad | BPMN-centric model | Forces BPMN as the workflow definition format, which conflicts with the chosen Serverless Workflow Specification DSL ([ADR-0003](0003-cpt-cf-serverless-runtime-adr-workflow-dsl.md)) |
| Bad | Heavier operational footprint | Zeebe requires its own cluster infrastructure with significant memory and storage requirements |

### Option D: Restate

Restate is a newer durable execution framework that uses a distributed log for state management, offering lower-latency step execution.

| | Aspect | Note |
|---|--------|------|
| Good | Low-latency step execution | Distributed log architecture reduces per-step overhead |
| Good | Rust SDK available | Early-stage Rust SDK exists |
| Good | Simpler operational model | Single binary deployment, lighter infrastructure footprint |
| Bad | Early-stage maturity | Fewer production deployments and smaller community compared to Temporal |
| Bad | Less proven long-running workflow support | Supports virtual objects and timers for long-running flows, but fewer production references for multi-day workflows compared to Temporal |
| Bad | No built-in schedule API | Schedule management would need to be built on top |
| Bad | Smaller ecosystem | Fewer integrations, patterns, and production-proven recipes |

### Option E: AWS Step Functions

AWS Step Functions provides serverless workflow orchestration using JSON-based state machine definitions (Amazon States Language).

| | Aspect | Note |
|---|--------|------|
| Bad | Not self-hostable | Fully managed only — conflicts with on-premises requirement (BR-035) |
| Neutral | Native AWS integration | Deep integration with AWS services, but creates vendor dependency |
| Bad | Vendor lock-in | Tightly coupled to AWS; no self-hosted deployment option — violates self-hostability requirement |
| Bad | No Rust SDK for workflow definition | Workflows defined in JSON (ASL), not via Rust SDK; activities require Lambda or other AWS compute |
| Bad | Limited compensation support | No native saga pattern; compensation requires manual implementation |
| Bad | Execution duration limits | Standard Workflows have a 1-year limit; Express Workflows limited to 5 minutes |
| Bad | Multi-tenant isolation model mismatch | AWS account/IAM boundaries don't align with the platform's GTS-based tenant model |

### Option F: Apache Airflow

Apache Airflow provides DAG-based workflow scheduling and orchestration, primarily designed for data pipeline and ETL workloads.

| | Aspect | Note |
|---|--------|------|
| Good | Mature and widely adopted | Large community, extensive operator ecosystem |
| Good | Strong scheduling capabilities | Advanced cron scheduling with backfill and catch-up |
| Good | Self-hostable | Open-source, no vendor lock-in |
| Bad | Python-centric | DAGs defined in Python; no Rust SDK — integration requires REST API wrapper with significant impedance mismatch |
| Bad | Not designed for durable execution | DAGs are scheduled batch jobs, not event-driven durable workflows; no native suspend/resume or event-waiting |
| Bad | No saga/compensation support | No built-in compensation pattern; rollback logic must be manually implemented |
| Bad | High latency per task | Scheduler-based dispatch adds seconds of latency per task, far exceeding the p95 <=50ms step dispatch target |

## More Information

### Decision Review Triggers

This decision should be revisited if any of the following occur:

* Temporal Rust SDK (`temporal-sdk-core`) is deprecated, archived, or receives no releases for >6 months
* Temporal Server licensing changes from MIT (see [LICENSE](https://github.com/temporalio/temporal/blob/main/LICENSE)) to a restrictive or commercial-only license
* Restate or another engine achieves a production-quality Rust SDK with built-in schedule API and long-running workflow support
* Operational cost of Temporal Server infrastructure becomes disproportionate to platform infrastructure cost

### Security Note

Temporal Server adds a network-accessible infrastructure component (gRPC Frontend) that requires security hardening. Temporal supports mTLS and custom authorization plugins; in self-hosted deployments, no authentication is enforced by default. Platform security context (`tenant_id`, `user_id`) must be propagated to Temporal via workflow metadata to maintain tenant isolation at the engine level.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **Related ADR**: [ADR-0003: Serverless Workflow Specification as Workflow DSL](0003-cpt-cf-serverless-runtime-adr-workflow-dsl.md) — the DSL that this engine interprets and executes

This decision directly addresses the following requirements and design elements:

* `cpt-cf-serverless-runtime-fr-execution-engine` — Durable execution with checkpointing, suspend/resume, long-running workflows
* `cpt-cf-serverless-runtime-fr-tenant-registry` — Multi-tenant isolation via application-layer tenant scoping
* `cpt-cf-serverless-runtime-fr-trigger-schedule` — Schedule API for cron/interval triggers with missed schedule policies
* `cpt-cf-serverless-runtime-fr-runtime-capabilities` — Runtime capabilities exposed as Temporal activities
* `cpt-cf-serverless-runtime-fr-execution-lifecycle` — Full invocation lifecycle: start, cancel, retry, suspend, resume, compensate
* `cpt-cf-serverless-runtime-fr-execution-visibility` — Queryable execution status and history via Visibility API
* `cpt-cf-serverless-runtime-fr-debugging` — Workflow history and replay for execution debugging
* `cpt-cf-serverless-runtime-fr-advanced-patterns` — Child workflows, parallel execution, saga/compensation, idempotency
* `cpt-cf-serverless-runtime-fr-governance-sharing` — Publishing governance and access control integration
* `cpt-cf-serverless-runtime-fr-replay-visualization` — Deterministic replay and history events for timeline visualization
* `cpt-cf-serverless-runtime-fr-deployment-safety` — Version pinning and task queue routing for blue-green deployment
* `cpt-cf-serverless-runtime-nfr-security` — Security context propagation, tenant isolation, audit trail; see [Security Note](#security-note)
* `cpt-cf-serverless-runtime-nfr-resource-governance` — Timeout enforcement, per-tenant quotas, worker pool sizing
* `cpt-cf-serverless-runtime-nfr-reliability` — Event-sourced execution for RTO/RPO targets
* `cpt-cf-serverless-runtime-nfr-ops-traceability` — Correlation ID and trace ID propagation via Temporal headers
* `cpt-cf-serverless-runtime-nfr-observability` — OpenTelemetry integration via SDK interceptors
* `cpt-cf-serverless-runtime-nfr-retention` — Per-tenant execution history retention
* `cpt-cf-serverless-runtime-nfr-performance` — Async activity execution for step dispatch latency targets
* `cpt-cf-serverless-runtime-nfr-scalability` — Horizontal scaling via sharded history/matching services
* `cpt-cf-serverless-runtime-nfr-tenant-isolation` — Application-layer tenant scoping prevents cross-tenant interference
* `cpt-cf-serverless-runtime-nfr-composition-deps` — Child workflow and activity dependency management
* `cpt-cf-serverless-runtime-principle-pluggable-adapters` (DESIGN) — Engine registers as a ToolKit runtime plugin bound to its GTS plugin type
* `cpt-cf-serverless-runtime-principle-impl-agnostic` (DESIGN) — Engine is one of multiple possible runtime plugin implementations
* `cpt-cf-serverless-runtime-principle-gts-identity` (DESIGN) — Temporal workflow IDs derived from GTS instance addresses
* `cpt-cf-serverless-runtime-principle-unified-callable` (DESIGN) — Functions and workflows invoked through the same trait
* `cpt-cf-serverless-runtime-usecase-resource-provisioning` — Multi-step provisioning with saga rollback
* `cpt-cf-serverless-runtime-usecase-tenant-onboarding` — Long-running onboarding with signal-based suspension
* `cpt-cf-serverless-runtime-usecase-subscription-lifecycle` — Scheduled subscription workflows via Schedule API
