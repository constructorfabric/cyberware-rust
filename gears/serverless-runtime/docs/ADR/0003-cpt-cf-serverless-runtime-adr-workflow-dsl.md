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
PURPOSE: Capture WHY the Serverless Workflow Specification was chosen as the
workflow definition language for the Serverless Runtime.

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
# ADR — Serverless Workflow Specification as Workflow DSL


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Option A: Serverless Workflow Specification (CNCF)](#option-a-serverless-workflow-specification-cncf)
  - [Option B: Amazon States Language (ASL)](#option-b-amazon-states-language-asl)
  - [Option C: BPMN 2.0](#option-c-bpmn-20)
  - [Option D: Custom DSL](#option-d-custom-dsl)
  - [Option E: Code-only (no declarative DSL)](#option-e-code-only-no-declarative-dsl)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-serverless-runtime-adr-workflow-dsl`
## Context and Problem Statement

The Serverless Runtime needs a workflow definition language that workflow authors use to express orchestration logic — sequential steps, conditional branching, loops, parallel execution, error handling, and sub-workflow composition. This language defines how workflows are authored, validated, and submitted to the platform.

The choice of definition language is separable from the choice of execution engine. The DSL defines the workflow format and validation rules; the engine interprets and executes it. A vendor-neutral DSL allows the workflow format to remain stable even if the execution engine changes.

## Decision Drivers

* The specification language should be vendor-neutral to avoid coupling the workflow format to a specific engine
* The specification language should use a declarative format (JSON/YAML) for readability and tooling compatibility
* The specification language must support the control-flow constructs required by the PRD: sequential steps, conditional branching, loops, parallel execution, error handling, and sub-workflow composition (BR-010, BR-104, BR-105). Compensation support (BR-133) must be achievable through task composition
* The specification must support validation of workflow definitions before execution, with actionable error feedback (BR-011)
* The format must be interpretable at runtime without compilation, so that workflows can be submitted and modified through an API without code deployments (BR-001)
* The format must support versioning and schema evolution as workflow capabilities expand

## Considered Options

* **Option A**: Serverless Workflow Specification (CNCF)
* **Option B**: Amazon States Language (ASL)
* **Option C**: BPMN 2.0 (Business Process Model and Notation)
* **Option D**: Custom DSL (platform-specific workflow language)
* **Option E**: Code-only (no declarative DSL)

## Decision Outcome

Chosen option: **"Option A: Serverless Workflow Specification (CNCF)"**, because it is a vendor-neutral, CNCF-backed standard with a well-defined JSON/YAML schema that covers all required control-flow constructs (sequential, conditional, parallel, error handling, sub-workflows). Compensation patterns can be achieved through task composition (Try/Raise). It decouples the workflow definition format from the execution engine, enabling workflow portability. Its declarative JSON/YAML format supports validation at submission time and is suitable for programmatic generation and visualization.

### Consequences

* Workflow validation is built against the CNCF Serverless Workflow JSON Schema rather than a proprietary format. Workflow definitions are validated at submission time.
* Workflow definitions are authored in JSON or YAML following the Serverless Workflow Specification. The specification defines 12 task types.
* The specification is at v1.0.0 (released January 2025). Workflow definitions should declare their specification version for forward compatibility as the specification evolves.
* Expression evaluation uses jq as the default expression language, as specified by the Serverless Workflow Specification.
* The DSL supports declarative authentication definitions, enabling workflow authors to declare authentication requirements alongside workflow logic.

### Confirmation

* A sample workflow definition is correctly parsed and validated against the Serverless Workflow Specification schema
* Validation rejects workflows using unsupported task types with clear error messages identifying the unsupported type
* jq expressions within workflow definitions are syntactically valid and parseable
* Workflow definitions round-trip through JSON/YAML serialization without data loss

## Pros and Cons of the Options

### Option A: Serverless Workflow Specification (CNCF)

The Serverless Workflow Specification is a CNCF project that defines a vendor-neutral, declarative workflow language in JSON/YAML. It covers function invocation, event handling, branching, parallel execution, error handling, and sub-workflow composition. Compensation patterns can be implemented through task composition (Try/Raise).

| | Aspect | Note |
|---|--------|------|
| Good | Vendor-neutral, CNCF-backed standard | No lock-in to a specific engine; workflows are portable across compliant runtimes |
| Good | Comprehensive control-flow constructs | Supports all required patterns: sequential (Do), conditional (Switch), parallel (Fork), error handling (Try), sub-workflows, loops (For), event-driven (Listen/Emit) |
| Good | Declarative JSON/YAML format | Suitable for validation at submission time and programmatic generation |
| Good | SDKs in major languages | SDKs available for Go, Java, .NET, Python, Rust, TypeScript; project is CNCF-backed with growing adoption |
| Neutral | Declarative verbosity | YAML/JSON definitions are more verbose than code-based alternatives; this is the trade-off for machine-readable validation and tooling support |
| Bad | Relatively new standard (v1.0.0, Jan 2025) | Ecosystem is still growing; fewer production deployments compared to established alternatives |

### Option B: Amazon States Language (ASL)

Amazon States Language is the JSON-based workflow definition format used by AWS Step Functions.

| | Aspect | Note |
|---|--------|------|
| Good | Mature ecosystem | Extensive production deployments via AWS Step Functions; well-documented patterns for common orchestration scenarios |
| Bad | AWS-specific | Tightly coupled to AWS Step Functions; no independent standard body governance |
| Bad | Limited control-flow | Saga/compensation is achievable via Catch/Retry patterns (AWS publishes official guidance) but is not a first-class construct; error handling (Retry with backoff/jitter, Catch with pattern matching) is capable but less structured than Serverless Workflow Spec's Try task |
| Bad | No community governance | Amazon controls the specification; no open contribution model |

### Option C: BPMN 2.0

Dismissed early. BPMN is a visual modeling standard — the XML format is generated by graphical editors, not authored by hand or submitted via API. This is a fundamentally different paradigm from a text-based DSL for API-driven workflow registration.

### Option D: Custom DSL

Dismissed early. Building a custom DSL offers perfect fit but requires designing, documenting, and maintaining a language from scratch — with no existing ecosystem, tooling, or portability. The effort is not justified when an existing standard covers the required constructs.

### Option E: Code-only (no declarative DSL)

Dismissed early. Without a declarative DSL, workflows cannot be submitted or modified through an API without code deployments — directly contradicting BR-001. Code-only also couples workflow definitions to the engine SDK, eliminating portability.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements and design elements:

* `cpt-cf-serverless-runtime-fr-runtime-authoring` — Serverless Workflow Specification provides the declarative format for runtime creation, modification, and registration of workflow definitions with schema-based validation feedback
* `cpt-cf-serverless-runtime-fr-input-security` — Workflow definitions are validated against defined schemas before execution begins; the DSL's JSON Schema enables structural validation at submission time
* `cpt-cf-serverless-runtime-fr-advanced-patterns` — DSL supports child workflows (Do with sub-workflow reference), parallel execution (Fork), conditional branching (Switch), loops (For), error handling (Try), and compensation patterns (achievable through Try/Raise composition)
* `cpt-cf-serverless-runtime-fr-debugging` — Declarative workflow structure supports dry-run validation of definition validity before execution
* `cpt-cf-serverless-runtime-fr-replay-visualization` — Declarative workflow structure can be parsed to visualize execution blocks and decision branches
* `cpt-cf-serverless-runtime-principle-impl-agnostic` (DESIGN) — Vendor-neutral DSL ensures workflow definitions are portable across execution engine adapters; workflow format is decoupled from engine choice
