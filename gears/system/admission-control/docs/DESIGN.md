# Technical Design — Admission Control


<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Principles & Constraints](#2-principles--constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Domain Model](#31-domain-model)
  - [3.2 Component Model](#32-component-model)
  - [3.3 API Contracts](#33-api-contracts)
  - [3.4 Internal Dependencies](#34-internal-dependencies)
  - [3.5 External Dependencies](#35-external-dependencies)
  - [3.6 Interactions & Sequences](#36-interactions--sequences)
  - [3.7 Database schemas & tables](#37-database-schemas--tables)
  - [3.8 Deployment Topology](#38-deployment-topology)
- [4. Additional context](#4-additional-context)
  - [4.1 Telemetry surface](#41-telemetry-surface)
  - [4.2 Security boundaries and threat model](#42-security-boundaries-and-threat-model)
  - [4.3 Testability and test strategy](#43-testability-and-test-strategy)
  - [4.4 Known design risks](#44-known-design-risks)
  - [4.5 Areas recorded as not applicable](#45-areas-recorded-as-not-applicable)
- [5. Traceability](#5-traceability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-admission-control-design-admission-control`
## 1. Architecture Overview

### 1.1 Architectural Vision

The gate is an evaluator and a router, not a service with state. Its decision path holds two stages: evaluation of the built-in policies compiled from configuration at startup, and one call to the selected policy engine. There is no store, no cache with an invalidation problem, and nothing on the path that can change between two requests except the engine's answer. That is what makes a 5 ms overhead budget an allocation rather than an aspiration: the only work the gate does for itself is bounded evaluation of a small set of already-compiled content, and the bound is what holds the figure rather than the set being cheap.

Everything else in the design is failure handling. Because the gate fails closed, the interesting behaviour is not what happens when the engine answers but what happens when it does not, and the design converges every such path — unreachable, timed out, errored, unmappable, absent, backing off — on one refusal constructor that stamps the could-not-run cause. That constructor is the only way to build a refusal from a failure, which makes the property in `cpt-cf-admission-control-nfr-fail-closed` structural rather than a matter of remembering to handle each case.

That thinness raises a fair question the documents should answer rather than imply: why a gear at all, when the caller-side helper pattern — `PolicyEnforcer` in `authz-resolver-sdk` — already exists for reaching a pluggable decision point from a domain gear. Three reasons. A library cannot be deployed out-of-process, and this component is one a deployment may want to run, scale, and secure separately. A library cannot serve the operational surface an operator needs to see which built-in policies loaded and which engine resolved. And the platform already has this exact shape as a gear: `authz-resolver` is a few hundred lines that select a plugin and route to it, with its caller-side helper in the SDK beside it — which is the arrangement here, and the precedent for it.

The third shaping force is what the gate refuses to do. It never modifies a request, so the request value it receives is passed to the engine and dropped; nothing in the design writes to it, and the response type carries no field through which a modification could travel. It never stores policy content, so built-in policies arrive as configuration and are compiled once. Both are absences rather than mechanisms, and both are what keep this gear from becoming the second policy engine its own risk register warns about.

### 1.2 Architecture Drivers

Requirements that significantly influence architecture decisions.

#### Functional Drivers

| Requirement | Design Response |
|-------------|------------------|
| `cpt-cf-admission-control-fr-admission-interface` | `cpt-cf-admission-control-component-admission-service` exposes one method pair, single and batch; the response type has no field capable of carrying a modified request, and one collection that carries obligations through from the engine untouched. |
| `cpt-cf-admission-control-fr-request-authenticity` | The security context is a required parameter of the admission trait rather than an ambient value. `cpt-cf-admission-control-component-admission-service` takes the subject and its tenant from it and refuses a mismatch through the verdict builder's own arm for that cause, before any built-in policy runs; `cpt-cf-admission-control-component-engine-client` forwards the same context and has no path that constructs one, so the identity the engine derives is the one the caller presented. |
| `cpt-cf-admission-control-fr-decision-order` | The service runs `cpt-cf-admission-control-component-builtin-evaluator` before `cpt-cf-admission-control-component-engine-client`, and returns on the first built-in-policy prohibition without constructing an engine request. |
| `cpt-cf-admission-control-fr-refusal-cause` | `cpt-cf-admission-control-component-verdict-builder` is the sole constructor of a refusal, and takes the cause as a required argument rather than defaulting it. |
| `cpt-cf-admission-control-fr-deferral-relay` | The engine result type carries a deferral variant from the first version, and the verdict reserves a third value that no code path constructs yet; `cpt-cf-admission-control-component-engine-client` maps a deferral as a **result**, not through the failure funnel, so it never reaches the could-not-run constructor, and `cpt-cf-admission-control-component-verdict-builder` stamps the awaiting-approval cause instead. |
| `cpt-cf-admission-control-fr-batch-admission` | `cpt-cf-admission-control-component-batch-combiner` folds member verdicts under an absorbing refusal, collecting every refused member rather than short-circuiting. |
| `cpt-cf-admission-control-fr-deferral-verdict` | The reserved third value becomes constructible in `cpt-cf-admission-control-component-verdict-builder`, and `cpt-cf-admission-control-component-batch-combiner` gains one ordering rule — refusal absorbs, then deferral, then admission — leaving the fold order-independent. No component gains state, because nothing here holds a deferred operation open. |
| `cpt-cf-admission-control-fr-remote-decision-surface` | The admission client is a trait over a service that holds no in-process assumption, so the remote projection is an added transport binding rather than a second implementation; `cpt-cf-admission-control-component-admission-service` is unchanged by it. |
| `cpt-cf-admission-control-fr-builtin-policy-form` | `cpt-cf-admission-control-component-policy-set` compiles each configured policy through the evaluation facility at startup; `cpt-cf-admission-control-component-builtin-evaluator` evaluates the compiled set and maps anything that is not a prohibition to "fall through to the engine". |
| `cpt-cf-admission-control-fr-builtin-policy-precedence` | Built-in policies are evaluated before the engine and a prohibition returns immediately; no code path lets an engine result overturn one. |
| `cpt-cf-admission-control-fr-builtin-policy-independence` | The built-in policy set is built from configuration and the types registry only, with no reference to the engine; it is populated even when engine resolution fails. The named reserved-name-prefix fixture is what makes that observable: the conformance suite runs it against two stub engines and against an engine-less deployment, so the property fails a test rather than going unchallenged. |
| `cpt-cf-admission-control-fr-builtin-evaluation-bounds` | `cpt-cf-admission-control-component-builtin-evaluator` wraps each policy evaluation in its configured bound and the set in a total bound; it constructs the evaluation context from the request and a gate-supplied timestamp, passing no handle of any kind. Where the backend's bound is cooperative it also sets the check interval rather than inheriting a default, since the overrun is one work unit wide and is spent from the gate's own overhead. Determinism is not enforced here but at startup, by `cpt-cf-admission-control-component-config-validator`. |
| `cpt-cf-admission-control-fr-engine-selection` | `cpt-cf-admission-control-component-engine-client` resolves one GTS instance at startup and holds it; no per-request resolution and no second candidate is retained. |
| `cpt-cf-admission-control-fr-absent-engine` | No engine configured starts the gear in a degraded state reported by `cpt-cf-admission-control-component-operational-api`, refusing with could-not-run on every request that no built-in policy already prohibits; a configured identifier that will not resolve fails startup in the config validator instead. Degraded rather than unready, because in-process the host is shared with the gears being gated. |
| `cpt-cf-admission-control-fr-engine-result` | The engine's permission cause is copied into the record and never read by the verdict builder, so no branch on it exists to be added by accident. |
| `cpt-cf-admission-control-fr-fail-closed` | Every engine-facing error converges on the refusal constructor; the configuration type has no field that could express an admit-on-failure setting. |
| `cpt-cf-admission-control-fr-engine-bound` | The engine call is wrapped in a configured timeout; expiry is one of the conditions the refusal constructor accepts. |
| `cpt-cf-admission-control-fr-engine-backoff` | A back-off signal opens a gate-side window during which calls are throttled and requests refuse without reaching the engine. |
| `cpt-cf-admission-control-fr-admission-records` | `cpt-cf-admission-control-component-recorder` owns the field set and its single builder; every other component contributes fields through it. |
| `cpt-cf-admission-control-fr-record-path-failure` | `cpt-cf-admission-control-component-recorder` owns the condition, because buffer age is its own signal: on sink unreachability, an aged oldest record, or a full buffer it reports and the admission service refuses from then on. Those refusals are counted rather than buffered — enqueueing them would need the capacity whose exhaustion caused them — and one gap record naming the interval and the count is published when the drain recovers. No component holds a path that drops, samples, or truncates a record to relieve pressure. |
| `cpt-cf-admission-control-fr-record-confidentiality` | The record type carries property names as a collection of strings and has no field of a type that could hold a value, so exclusion is structural. |
| `cpt-cf-admission-control-fr-subject-data-handling` | The record type carries the subject as the opaque reference the security context supplies and has no field able to hold a profile attribute. `cpt-cf-admission-control-component-recorder` exposes no delete, rewrite, or resolve path, and needs none: erasure belongs to the identity provider, and nothing in a published record changes when it happens. |
| `cpt-cf-admission-control-fr-operational-surface` | `cpt-cf-admission-control-component-operational-api` reads the built-in policy set and the engine handle directly; it holds no state of its own and registers no route that mutates either. Refusal counts come from bounded in-memory counters over a recent window, kept by the same component that constructs a refusal, so no path writes them twice and none of them is durable state — the durable evidence is the admission record on the audit topic. |
| `cpt-cf-admission-control-fr-metrics` | Telemetry surface in Section 4.1, emitted by the component owning each measurement. |
| `cpt-cf-admission-control-fr-configuration-validation` | Configuration is a deny-unknown-fields structure validated during the init phase, including syntax validation of every built-in policy through the evaluation facility, resolution of every resource type those policies name, resolution of the engine identifier, and a screen of each policy's parsed form for builtins denylisted for the backend build in use. |

#### NFR Allocation

This table maps non-functional requirements from PRD to specific design/architecture responses.

| NFR ID | NFR Summary | Allocated To | Design Response | Verification Approach |
|--------|-------------|--------------|-----------------|----------------------|
| `cpt-cf-admission-control-nfr-overhead` | p95 5 ms, p99 10 ms excluding engine time | `cpt-cf-admission-control-component-policy-set`, `-builtin-evaluator` | Content compiled once at startup, so no request parses anything; selection is a hash lookup plus a pattern check with no I/O. Evaluation of the selected policies is the dominant term in the figure, which is why the per-policy and total bounds of `cpt-cf-admission-control-fr-builtin-evaluation-bounds` are what keep it inside budget rather than the selection cost | Benchmark at the gate boundary with engine time subtracted; per-stage timing histograms separating selection from evaluation |
| `cpt-cf-admission-control-nfr-fail-closed` | Zero admissions on any failure | `cpt-cf-admission-control-component-verdict-builder` | One refusal constructor; admission is reachable only from an engine permission, never from an error branch and never from a built-in policy | Fault injection across the enumerated conditions |
| `cpt-cf-admission-control-nfr-non-modification` | Request unchanged after the call | `cpt-cf-admission-control-component-admission-service` | The request is taken by shared reference and never cloned into the response; the response type has no request-shaped field | Conformance test comparing the caller's request before and after, on both verdicts |
| `cpt-cf-admission-control-nfr-availability` | No host impact in-process; 99.95 percent out-of-process | `cpt-cf-admission-control-topology-in-process` | Nothing on the decision path runs in the background, holds an unbounded queue, or holds a lock across an await. One background task exists off that path — the recorder's drain in `cpt-cf-admission-control-component-recorder` — and it is bounded by the buffer it drains, with its failures caught at the task boundary so a drain panic degrades the record path per `cpt-cf-admission-control-fr-record-path-failure` rather than reaching the host. The one resource that could otherwise grow without bound is that record buffer, which is bounded | Fault injection asserting zero host-fatal outcomes, including a panic injected into the drain task; availability measurement in the out-of-process shape |
| `cpt-cf-admission-control-nfr-record-completeness` | One record per decision, ≤5 s awaiting durability, with one accounted exception | `cpt-cf-admission-control-component-recorder` | Records appended to a bounded in-memory buffer on the decision path and drained by a separate task; buffer age and occupancy exported. The exception is the serving condition of `cpt-cf-admission-control-fr-record-path-failure`: rather than sampling or dropping under sink failure, the gate refuses and accounts for the interval with one gap record, so every decision is either recorded or inside a stated gap | Throughput test asserting record count equals decision count; fault-injected sink outage and buffer saturation asserting refusal rather than admission, no dropped record, and exactly one gap record on recovery |

#### Key ADRs

No decision record exists for this gear yet. The decisions below are the ones this design takes that warrant one. Identifiers are omitted until the records exist, because allocating them before produces dangling references that the repository's deterministic validation rejects.

| Planned decision | Decision Summary |
|--------|-----------------|
| Single engine selection | Select exactly one policy engine per deployment rather than combining verdicts from several, following the platform's existing plugin-selection idiom and declining the Kubernetes multi-webhook model. |
| Built-in policies evaluated in the gate | Evaluate the platform's own policies here, through the shared evaluation facility, rather than delegating them to the selected engine — which is what makes them survive substitution of that engine. Management stays out: they arrive as deployment configuration, with no authoring surface. |
| Fail closed without bypass | Refuse on every engine failure and expose no configuration that converts a failure into an admission. |
| Judgement without modification | Return a verdict only, leaving request enrichment to the calling gear's own pipeline. |
| Gear rather than caller-side library | Ship as a gear with an SDK beside it, rather than as a library each enforcing gear links, so that the component can be deployed out-of-process and can serve an operational surface — following `authz-resolver`, which is the same shape. |

### 1.3 Architecture Layers

```mermaid
graph TD
    EG[Enforcing gear]
    OP[Platform operator]
    CFG[Deployment configuration]
    subgraph Presentation
        REST[Operational REST surface]
    end
    subgraph Application
        ACL[Admission client]
        CV[Config validator]
    end
    subgraph Domain
        SEQ[Decision sequence]
    end
    subgraph Infrastructure
        PS[Built-in policy compilation and evaluation, via the evaluation facility]
        ECL[Engine client]
        REC[Recorder]
    end
    EG --> ACL
    OP --> REST
    CFG --> CV
    TR[types-registry] --> CV
    CV --> PS
    CV --> ECL
    ACL --> SEQ
    SEQ --> PS
    SEQ --> ECL
    SEQ --> REC
    REST --> PS
    REST --> ECL
    ECL --> PE[Selected policy engine]
    REC -.->|async| BR[event-broker audit topic]
```

Nodes inside the layer boxes are this gear's own code and the libraries it links, the evaluation facility among them. Nodes outside are separate gears and external systems, reached rather than linked — and there is no store among them, which is the gear's defining property.


- [ ] `p1` - **ID**: `cpt-cf-admission-control-tech-stack`

| Layer | Responsibility | Technology |
|-------|---------------|------------|
| Presentation | A read-only operational surface — readiness, selected engine, loaded built-in policies and match counts — present in both deployment shapes. The decision surface is projected here only in the out-of-process shape | `toolkit-http`, `toolkit-canonical-errors` |
| Application | The admission client registered in ClientHub, the engine plugin contract, gear lifecycle, configuration | ToolKit gear capabilities, ClientHub |
| Domain | Built-in policy selection, verdict and cause construction, batch combination, record construction | Rust, `#[domain_model]` types |
| Infrastructure | Built-in policy compilation and evaluation, engine resolution and invocation, types-registry resolution, record emission | Evaluation facility, ClientHub scoped resolution, `toolkit-gts`, `toolkit-canonical-errors` |

## 2. Principles & Constraints

### 2.1 Design Principles

#### Decide Nothing That Can Be Delegated

- [ ] `p1` - **ID**: `cpt-cf-admission-control-principle-thin`

The gate answers one question and owns only what that question needs: the interface, the ordering, the failure semantics, and the built-in policies. Policy semantics belong to the engine, request enrichment belongs to the calling gear, and content lifecycle belongs to neither. Every capability this gear does not have is load-bearing — the split that created it exists because a gateway that manages policy is a policy engine with a different name.

**Planned decision record**: Built-in policies evaluated in the gate

#### Compiled at Startup, Evaluated at Request Time

- [ ] `p1` - **ID**: `cpt-cf-admission-control-principle-compile-at-startup`

Built-in policies arrive as configuration, are validated and compiled at startup through the evaluation facility, and are evaluated at request time against the request context. Compiling once is what keeps parsing off the decision path; the cost that remains is evaluation itself, bounded per policy and in total. The gate holds the same isolation obligation the engine does — the backend receives the context, the content, and a supplied timestamp, and no capability.

**Planned decision record**: Built-in policies evaluated in the gate

#### One Way to Refuse From a Failure

- [ ] `p1` - **ID**: `cpt-cf-admission-control-principle-single-refusal`

Every path that cannot obtain an answer — no engine, unreachable engine, timeout, error, unmappable result, active back-off — converges on a single constructor that stamps the could-not-run cause. Admission is reachable only from an engine permission. Making this structural rather than conventional is what stops a new failure path from silently defaulting to admit, which is the one defect in this gear that would be invisible in production.

**Planned decision record**: Fail closed without bypass

#### The Request Belongs to the Caller

- [ ] `p1` - **ID**: `cpt-cf-admission-control-principle-no-modification`

The gate borrows the request, reads it, and returns a verdict. It does not clone it into the response, and the response type has no field shaped to carry one back. A caller's validation of its own request is therefore never conditional on what the gate did with it.

**Planned decision record**: Judgement without modification

#### Nothing on the Decision Path Changes Between Requests

- [ ] `p2` - **ID**: `cpt-cf-admission-control-principle-stateless-path`

After startup, the built-in policy set and the engine handle are immutable for the life of the process. The decision path performs no read that could return different data to two concurrent requests, which is what makes the gate's contribution to latency flat and its behaviour reproducible from configuration alone.

### 2.2 Constraints

#### No Management API for Built-in Policies

- [ ] `p1` - **ID**: `cpt-cf-admission-control-constraint-no-builtin-policy-api`

Built-in policies are supplied by deployment configuration and change with the deployment. The gear exposes no endpoint and no client method that creates, modifies, or withdraws one. This is the boundary the gear exists to hold: a runtime-authorable built-in policy set here would duplicate the policy engine's lifecycle, versioning, and audit with a second model.

#### Exactly One Engine

- [ ] `p1` - **ID**: `cpt-cf-admission-control-constraint-single-engine`

One policy engine is resolved at startup and held. The design carries no verdict-combination model, no engine ordering, and no per-engine failure policy, because none is reachable with a single engine. Adding a second engine later is a change to this constraint and to the requirements that rest on it, not a configuration change.

**Planned decision record**: Single engine selection

#### The Evaluation Facility Is a Hard Dependency

- [ ] `p1` - **ID**: `cpt-cf-admission-control-constraint-evaluation-facility`

The gate links the platform's evaluation facility, which does not yet exist. It shares that exposure with the policy engine rather than avoiding it: built-in policies are real policy content, and evaluating them is what makes them independent of the selected engine. What the gate does not take on is content **management** — no store, no lifecycle, no authoring surface — which is the boundary that matters, and it is unaffected by the dependency.

The audit of the first candidate backend settled two things the gate must therefore build for. Its cost bound is cooperative, so a per-policy limit overruns by up to one unit of work, spent from the gate's own overhead rather than the engine's. And it declares no sandbox posture of its own while registering builtins that read a clock or generate random values, none of which can be removed or shadowed — so determinism is a startup-time content check here, not a runtime guarantee obtained from the facility.

#### The Overhead Budget Is Borrowed

- [ ] `p2` - **ID**: `cpt-cf-admission-control-constraint-borrowed-budget`

The gate's 5 ms is a slice of the enforcing gear's own budget, taken alongside the engine's 25 ms. Any design change that adds a synchronous dependency to the decision path spends the caller's budget, which is why the path admits exactly two things: in-process evaluation of the compiled built-in set, under its own bounds, and one engine call.

#### In-Process at First Release

- [ ] `p2` - **ID**: `cpt-cf-admission-control-constraint-in-process`

The gate, its enforcing gears, and the selected engine share one process. The contracts are transport-agnostic, so an out-of-process deployment needs no contract change — but it needs the overhead and availability figures revisited, since neither contains an allowance for a transport hop.

## 3. Technical Architecture

### 3.1 Domain Model

**Technology**: Rust domain types under `#[domain_model]`; GTS identifiers for resource types, engine instances, and error families

**Location**: `gears/system/admission-control/admission-control/src/domain/`

**Core Entities**:

- [ ] `p1` - **ID**: `cpt-cf-admission-control-entity-admission-request`
- [ ] `p1` - **ID**: `cpt-cf-admission-control-entity-gated-operation`
- [ ] `p1` - **ID**: `cpt-cf-admission-control-entity-verdict`
- [ ] `p1` - **ID**: `cpt-cf-admission-control-entity-builtin-policy`
- [ ] `p1` - **ID**: `cpt-cf-admission-control-entity-policy-set`
- [ ] `p1` - **ID**: `cpt-cf-admission-control-entity-admission-record`
- [ ] `p1` - **ID**: `cpt-cf-admission-control-entity-engine-result`

| Entity | Description | Schema |
|--------|-------------|--------|
| AdmissionRequest | What an enforcing gear submits: subject and subject tenant, action, resource type and optional identifier, tenant context, and caller-supplied operation properties, alongside the caller's propagated security context. The subject fields are checked against that context and taken from it per `cpt-cf-admission-control-fr-request-authenticity`, so they restate an authenticated fact rather than asserting one. Borrowed by the gate, never retained. Carries no correlation identifier — the gate mints that itself. | Value type |
| GatedOperation | The gate's own view of one admission: the request plus the correlation identifier minted for it, and the batch identifier where it belongs to a batch. Created at entry, carried to the engine, and consumed by the record. | Value type |
| Verdict | Admitted or refused, with a cause on a refusal, and on an admission the engine's permission cause plus any obligations it attached. Refusal causes include the identity mismatch of `cpt-cf-admission-control-fr-request-authenticity`, which is a caller defect rather than an outage and is therefore distinct from could-not-run. Carries a reserved third value for a deferral, which no code path constructs until `cpt-cf-admission-control-fr-deferral-verdict` ships. The gate's whole answer. | Value type |
| BuiltinPolicy | One of the platform's own policies as configuration supplies it: a stable identity the refusal names, the resource types and operations it applies to, and policy content in a language a backend of the evaluation facility accepts. Yields a prohibition or nothing. | Configuration |
| BuiltinPolicySet | The compiled, immutable set built at startup — each policy's compiled content, plus a selection index keyed for constant-time lookup on the common path. | In-memory |
| EngineResult | What the engine returns: a permission with its cause and obligations, a prohibition with a reason, or a deferral. A back-off request is not a fourth variant — it is an optional signal carried **alongside** a result, and the result it accompanies is the engine's own transient failure, which the gate maps to could-not-run while `cpt-cf-admission-control-fr-engine-backoff` opens the window. Modelling it as a variant would make it a result the gate has to map, and the only mapping available would be the failure funnel, which is where it already lands by the shorter route. Mapped to a Verdict, never surfaced raw — but obligations are copied across untouched. The deferral variant exists from the first version even though no engine emits one, which is what keeps it out of the unmappable-result path. | Value type |
| AdmissionRecord | The durable projection of a decision, carrying the field set of `cpt-cf-admission-control-fr-admission-records`. One further shape shares the topic and is not a decision: the gap record of `cpt-cf-admission-control-fr-record-path-failure`, which names an interval and a count of refusals the gate could not record individually. | Published to the `event-broker` audit topic |

**Relationships**:
- AdmissionRequest → GatedOperation: each request becomes exactly one gated operation at entry, which is where the correlation identifier is minted.
- AdmissionRequest → Verdict: each request yields exactly one verdict, and a batch yields one verdict per member plus one combined.
- BuiltinPolicy → BuiltinPolicySet: the set is compiled from configuration once; a built-in policy has no existence at request time outside it.
- EngineResult → Verdict: mapped, never passed through, so no engine-specific shape reaches a caller.
- Verdict → AdmissionRecord: each verdict yields exactly one record.

### 3.2 Component Model

The gear is two crates: `admission-control-sdk`, carrying the admission client, the engine plugin contract, the shared models, and the error family; and `admission-control`, carrying the gate itself. Components below are modules, not processes.

```mermaid
graph LR
    AS[Admission service] --> BE[Built-in evaluator]
    AS --> EC[Engine client]
    AS --> VB[Verdict builder]
    AS --> BC[Batch combiner]
    AS --> REC[Recorder]
    BE --> PS[Built-in policy set]
    BE --> EF[Evaluation facility]
    CV[Config validator] --> PS
    CV --> EC
```

#### Admission Service

- [ ] `p1` - **ID**: `cpt-cf-admission-control-component-admission-service`

##### Why this component exists

Enforcing gears need one call. This is that call, and the only component they link.

##### Responsibility scope

Owns the decision sequence and its ordering: security-context check, then built-in evaluation, then engine call, then verdict, then record. The context check is first and cheap — the subject and its tenant come from the context, and a request asserting different ones refuses through the verdict builder's own arm for it, before a built-in policy is evaluated and without an engine request being constructed. Implements `cpt-cf-admission-control-interface-admission-client` for single and batch requests. Holds the request by shared reference for the duration of the call and returns without retaining it.

##### Responsibility boundaries

Evaluates no policy, stores no content, and constructs no refusal itself — it delegates that to the verdict builder so that every refusal has one origin. Does not enforce its own verdict and has no way to observe whether a caller did.

##### Related components (by ID)

- `cpt-cf-admission-control-component-builtin-evaluator` — calls
- `cpt-cf-admission-control-component-engine-client` — calls
- `cpt-cf-admission-control-component-verdict-builder` — calls
- `cpt-cf-admission-control-component-batch-combiner` — calls
- `cpt-cf-admission-control-component-recorder` — publishes to

#### Built-in Policy Set

- [ ] `p1` - **ID**: `cpt-cf-admission-control-component-policy-set`

##### Why this component exists

Built-in policies must be applied on every gated operation without parsing content per request, without a selection cost that grows with the size of the set, and without touching anything outside the process.

##### Responsibility scope

Compiles each configured built-in policy through the evaluation facility at startup, holding the compiled forms in an immutable set alongside a selection index keyed by resource type, with namespace-wildcard entries held separately and checked only when the exact key misses. Resolves every concrete resource type through the types registry during compilation, so an unresolvable type fails startup rather than producing a policy that never fires.

##### Responsibility boundaries

Holds no request state and is never rebuilt after startup. Compilation and syntax failures surface at startup, so content that will not compile never reaches a request. It does not evaluate: a failure while evaluating compiled content at request time belongs to `cpt-cf-admission-control-component-builtin-evaluator` and refuses there.

##### Related components (by ID)

- `cpt-cf-admission-control-component-config-validator` — built by
- `cpt-cf-admission-control-component-builtin-evaluator` — owns data for

#### Built-in Evaluator

- [ ] `p1` - **ID**: `cpt-cf-admission-control-component-builtin-evaluator`

##### Why this component exists

It is the platform's own check, and the only part of the verdict the gate decides itself.

##### Responsibility scope

Selects the built-in policies applicable to a request from the index by resource type and operation, exact key first and wildcard second, then evaluates each selected policy through the evaluation facility under its configured per-policy bound and the whole selection under the total bound. Where the backend checks that bound cooperatively it sets the check interval explicitly, because the overrun is one work unit wide and is charged to the gate's overhead figure rather than the engine's. Constructs the evaluation context from the request and a gate-supplied timestamp, and passes no client, connection, handle, or clock. Returns the identity of the first policy that prohibits, or nothing. Deterministic in configuration order, so two policies prohibiting the same operation always name the same one.

##### Responsibility boundaries

Cannot admit. A selection that produces no prohibition yields no verdict at all, which the service reads as "proceed to the engine" rather than as a permission — the distinction that keeps built-in policies from becoming an admission path. Compiles nothing: it evaluates what `cpt-cf-admission-control-component-policy-set` compiled at startup. An exceeded bound or a backend failure is not a non-match — both converge on the refusal constructor with the could-not-run cause, because a policy that could not be evaluated is one whose prohibition cannot be ruled out.

##### Related components (by ID)

- `cpt-cf-admission-control-component-policy-set` — depends on
- `cpt-cf-admission-control-component-admission-service` — called by
- `cpt-cf-admission-control-component-verdict-builder` — refuses through, on a bound exceedance or a backend failure

#### Engine Client

- [ ] `p1` - **ID**: `cpt-cf-admission-control-component-engine-client`

##### Why this component exists

The engine is a remote-shaped dependency even when it is in-process, and every call to it needs a bound, a failure mapping, and a back-off state.

##### Responsibility scope

Resolves the configured engine's GTS instance from ClientHub once at startup and holds the handle. Forwards the caller's security context unchanged on every call, and has no path that constructs one, so the identity the engine derives is the identity the caller presented. Wraps each call in the configured timeout, maps every failure mode onto the could-not-run condition, and owns the back-off window a back-off signal opens — during which it refuses without calling.

##### Responsibility boundaries

Interprets no result beyond mapping it to an `EngineResult`; an engine answer it cannot map is a failure, not a guess. A deferral is mappable and is mapped, which is the point of carrying the variant before any engine emits one — an outcome the type cannot name would land in the failure funnel and be reported as an outage. Obligations pass through the mapping unexamined — the gate holds no registry of obligation identifiers and needs none, because recognising them is the enforcing gear's duty. Never retries within a request: a retry inside the caller's budget spends the caller's budget.

##### Related components (by ID)

- `cpt-cf-admission-control-component-verdict-builder` — calls
- `cpt-cf-admission-control-component-config-validator` — configured by

#### Verdict Builder

- [ ] `p1` - **ID**: `cpt-cf-admission-control-component-verdict-builder`

##### Why this component exists

The fail-closed property is worth having as a structural guarantee rather than as a convention observed at each call site.

##### Responsibility scope

The sole constructor of a Verdict. Takes a cause as a required argument for every refusal, and constructs an admission only from an engine permission. An identity mismatch is its own arm, distinct from the failure arm, because a caller asserting an identity it does not hold is a defect to be fixed in the caller rather than an outage to be retried. An engine deferral is constructed as a refusal carrying the awaiting-approval cause, from its own arm rather than from the failure arm, so the two can never be confused by a later edit; the reserved third value has no constructor at all until `cpt-cf-admission-control-fr-deferral-verdict` ships. Carries the engine's permission cause and its obligations onto the verdict without reading either. Obligations are copied by the same move that copies the cause, so there is no branch that could inspect one and no path that could drop one.

##### Responsibility boundaries

Performs no I/O and makes no decision of its own — it records the decision another component reached, in a form the caller can branch on. Its exclusivity is the invariant: no other module may construct a Verdict.

##### Related components (by ID)

- `cpt-cf-admission-control-component-admission-service` — called by

#### Batch Combiner

- [ ] `p1` - **ID**: `cpt-cf-admission-control-component-batch-combiner`

##### Why this component exists

A caller admitting a plan needs one answer plus enough detail to report every fault at once.

##### Responsibility scope

Folds member verdicts under an absorbing refusal and collects every refused member with its cause. Enforces the configured batch bound before any member is evaluated. The fold takes one further precedence once `cpt-cf-admission-control-fr-deferral-verdict` ships — refusal absorbs, then deferral, then admission — which keeps it order-independent and matches the engine's own combination.

##### Responsibility boundaries

A pure fold with no I/O, so batch semantics are testable without an engine. Does not short-circuit on the first refusal, because the caller needs the complete list — the cost of continuing is bounded by the batch limit.

##### Related components (by ID)

- `cpt-cf-admission-control-component-admission-service` — called by

#### Recorder

- [ ] `p1` - **ID**: `cpt-cf-admission-control-component-recorder`

##### Why this component exists

Every decision must leave evidence, and writing it synchronously would spend the overhead budget it is measured against.

##### Responsibility scope

Owns the record field set and its single builder. Appends to a bounded in-memory buffer on the decision path and drains it from a separate task, publishing to the `event-broker` audit topic. Exports buffer age and occupancy so the loss window is observable. The buffer is the only place a record is unpublished, so its occupancy is the whole of what a crash would lose — the gate holds no store behind it to recover from.

Owns the serving condition of `cpt-cf-admission-control-fr-record-path-failure` and the counters behind it. When the sink is unreachable, the oldest buffered record ages past the window, or the buffer fills, it reports the condition and the admission service refuses from then on; the refusals that condition produces are counted rather than buffered, because buffering them would need the capacity whose absence caused them. When the drain recovers, the recorder publishes one gap record naming the interval and the count. That record is the only thing it emits which is not a decision, and it exists so the hole is stated by the stream rather than inferred from a thinning of it.

Mints the correlation identifier at entry, before any built-in policy is evaluated and before the engine is reached, so that the identifier exists on every path that can produce a record — including the two paths on which no engine record will ever exist: a refusal by built-in policy, and a could-not-run refusal. Minting on entry rather than at record construction is what makes the identifier available to forward, and forwarding it is the whole of the join.

##### Responsibility boundaries

Does not decide, does not filter which decisions are recorded, and never writes a credential or a caller-supplied property value — the record type carries property names only, so exclusion is a property of the type rather than of a filtering step. Offers no path that deletes or rewrites a published record, and needs none: per `cpt-cf-admission-control-fr-subject-data-handling` the subject is written as the opaque identity-provider reference the context carries, and erasure happens at that provider without any record changing. A gear with no store can hold that position precisely because it makes no claim to rewrite anything.

##### Related components (by ID)

- `cpt-cf-admission-control-component-admission-service` — called by

#### Operational API

- [ ] `p1` - **ID**: `cpt-cf-admission-control-component-operational-api`

##### Why this component exists

Every other component here produces a decision whose inputs an operator cannot see. A built-in policy that never fires and an engine that resolved to something unexpected are both silent failures, and this is the only place they become visible.

##### Responsibility scope

Registers the gear's read-only routes and its readiness check — the latter matters structurally, because the platform hangs `healthcheck` off the same capability as route registration, so a gear with no routes has no way to report readiness at all. Reports the selected engine's identity, the loaded built-in policies with their identities and match counts, the degraded state when no engine is configured, and the record-path condition of `cpt-cf-admission-control-fr-record-path-failure` together with the refusals it has produced — which is the only place those are visible, since they are the refusals deliberately kept out of the record stream.

##### Responsibility boundaries

Reads the built-in policy set and the engine handle and holds no state of its own. Registers no route that mutates either, which is what keeps `cpt-cf-admission-control-constraint-no-builtin-policy-api` true in the presence of a REST surface. Reports content identities and counts, never content bodies, so the surface cannot become a way to read what the platform's own policies say. Exposes no admission decision: in the in-process shape the decision surface is a typed client, and in the out-of-process shape it is projected separately over the platform transport.

##### Related components (by ID)

- `cpt-cf-admission-control-component-policy-set` — reads
- `cpt-cf-admission-control-component-engine-client` — reads

#### Config Validator

- [ ] `p1` - **ID**: `cpt-cf-admission-control-component-config-validator`

##### Why this component exists

Every setting here changes what the platform refuses, and a setting that silently does nothing is the failure an operator cannot detect.

##### Responsibility scope

Runs during the gear's init phase. Rejects unknown keys, resolves the engine identifier and every resource type a built-in policy names through the types registry, validates each built-in policy's syntax through the evaluation facility independently of evaluating it, screens the parsed form for denylisted builtins, checks that bounds are present and positive, and fails startup on any of them. The denylist screen belongs here rather than at evaluation because a builtin can be neither removed from the backend nor shadowed by anything registered over its name, so startup is the last point at which the content can still be refused.

##### Responsibility boundaries

Does not construct the running components, only validates and hands over the validated values, so a partially valid configuration cannot produce a partially built gate.

##### Related components (by ID)

- `cpt-cf-admission-control-component-policy-set` — builds
- `cpt-cf-admission-control-component-engine-client` — configures

### 3.3 API Contracts

#### Admission Client

- **Realizes PRD interface**: `cpt-cf-admission-control-interface-admission-client`
- **Contracts**: `cpt-cf-admission-control-contract-admission-record`
- **Technology**: Rust async trait registered in ClientHub without scope
- **Location**: `gears/system/admission-control/admission-control-sdk/src/api.rs`

The surface enforcing gears link. Takes the caller's security context and the request by shared reference, returns a verdict by value. The context is a parameter rather than an ambient value precisely so that a call site cannot omit it and so that the subject cannot be read from anywhere else. There is no REST projection: the only caller is in-process, and a hop would spend the caller's budget.

**Endpoints Overview**:

| Method | Path | Description | Stability |
|--------|------|-------------|-----------|
| `n/a` | `admit` | One security context and one intended operation, one verdict, with obligations relayed on an admission | stable |
| `n/a` | `admit_batch` | Several members, one combined verdict plus per-member verdicts | stable |

#### Operational REST API

- **Realizes PRD interface**: `cpt-cf-admission-control-interface-operational-api`
- **Technology**: REST over the platform API prefix, RFC 9457 problem responses
- **Location**: `gears/system/admission-control/admission-control/src/api/rest/`

Read-only. Registering it is also what gives the gear a readiness carrier, since the platform hangs `healthcheck` off the same capability as route registration.

**Endpoints Overview**:

| Method | Path | Description | Stability |
|--------|------|-------------|-----------|
| `GET` | `/admission-control/v1/status` | Selected engine identity and degraded state | stable |
| `GET` | `/admission-control/v1/builtin-policies` | Loaded built-in policies with identities and match counts | stable |
| `GET` | `/admission-control/v1/refusals` | Recent-window refusal counts by built-in policy identity, by could-not-run failure condition, and by identity mismatch, reported separately from engine-decided refusals; includes the refusals produced while the record path was failing, which are counted here because they are deliberately not recorded individually | stable |

#### Policy Engine Plugin Contract

- **Realizes PRD interface**: `cpt-cf-admission-control-interface-engine-plugin`
- **Contracts**: `cpt-cf-admission-control-contract-gts`
- **Technology**: Rust async trait registered in ClientHub under a GTS instance scope
- **Location**: `gears/system/admission-control/admission-control-sdk/src/plugin_api.rs`

The contract an engine implements to be selectable. The gate owns it; the [Policy Engine](../../policy-engine/docs/PRD.md) is its first implementation.

**Endpoints Overview**:

| Method | Path | Description | Stability |
|--------|------|-------------|-----------|
| `n/a` | `evaluate` | One forwarded security context and one request, the latter carrying the correlation identifier minted here; a permission with cause and obligations, a prohibition with reason, or a deferral | unstable |
| `n/a` | `evaluate_batch` | Several requests, per-member results | unstable |

### 3.4 Internal Dependencies

| Dependency Gear | Interface Used | Purpose |
|-------------------|----------------|----------|
| Selected policy engine | Plugin contract via scoped ClientHub | Evaluating tenant-authored policy; the only component that does |
| `types-registry` | SDK client and GTS inventory | Resolving the engine identifier and every resource type a built-in policy names, at startup; registering the plugin specification and error family |
| `toolkit-canonical-errors` | Library | The gear's error family and the five refusal causes of `cpt-cf-admission-control-fr-refusal-cause`: refused by a built-in policy, refused by policy, awaiting approval, identity mismatch, and could-not-run |
| `toolkit-security` | Library | Security context propagation from the enforcing gear to the engine, and the source of the subject identity the gate derives rather than believes |
| `event-broker` | Publish to the audit topic | Durability, retention, and export of admission records; the gate holds no store, so publication is where a record becomes durable |

**Dependency Rules** (per project conventions):
- No circular dependencies
- Always use sdk modules for inter-gear communication
- No cross-category sideways deps except through contracts
- Only integration/adapter gears talk to external systems
- `SecurityContext` must be propagated across all in-process calls

**Shared type ownership.** Two of the schemas this gear reads are defined elsewhere and are worth naming, because the policy engine reads the same two and neither gear may define one. The declaration a backend *build* carries — the syntax-validation and imposed-cost-bound guarantees Section 2.2 depends on, plus the sandbox-and-determinism claim the gate refuses to run without — belongs to the evaluation facility's SDK, which is the only component that knows which features a build enables and which builtins it registers. The obligation base type, whose instances are the identifiers the gate relays and the enforcing gear honours, belongs in `libs/toolkit-gts` beside the authorization permission base type: the vocabulary crosses every gear on the path, so it cannot belong to a replaceable engine, and it should not belong to this gear's SDK either, since that would make an obligation-honouring gear depend on the gate in order to name what it is honouring. The policy engine's design states the same two positions and the reasoning behind them, and its PRD Section 13 carries the ownership question with platform architecture named as the owner — one question serving both gears, since a schema either of them owned would make the other depend on it. An answer that moves a schema changes only where both gears import it from.

### 3.5 External Dependencies

The gate owns no database, reads no file after startup, and links the evaluation facility as an in-process library rather than reaching a service for it. It has one external system: `event-broker`, which receives every admission record. That dependency is not incidental. Because the gate stores nothing itself, publication is the point at which a record becomes durable, so the broker's acceptance — not a local write — is what `cpt-cf-admission-control-nfr-record-completeness` measures. The gate publishes asynchronously and the broker acknowledges once the record is durable in its ingest outbox, which is what lets a gear with no store of its own still hold a bounded loss window.

That absence is the design's main lever on `cpt-cf-admission-control-nfr-overhead` and `cpt-cf-admission-control-nfr-availability`: a component with no external dependency cannot be slowed or stopped by one, so its own contribution to latency and to failure is bounded by its code rather than by anything it waits on. The one thing it does wait on — the engine — is a gear, not an external system, and is bounded by `cpt-cf-admission-control-fr-engine-bound`.

It also has a cost, and the cost is the serving condition. Because the topic is the durability point, a broker outage is not a logging problem here as it is for the policy engine, which still has its own table to fall back on. The gate has nothing behind the buffer, so the three properties of `cpt-cf-admission-control-fr-record-path-failure` — no sampling, a bounded window, and no blocking on the decision path — stop being jointly satisfiable the moment the sink is unreachable, and the design resolves that by refusing rather than by dropping. Two consequences follow for the components. The recorder owns the condition, because buffer age is the signal and it is the only component holding it; and the refusals the condition produces are counted rather than buffered, since enqueueing them would need the capacity whose exhaustion caused them. The gap record published on recovery is what keeps the resulting hole in the stream stated rather than silent, and it is the one payload here that does not describe a single decision.

### 3.6 Interactions & Sequences

#### Gate an operation

**ID**: `cpt-cf-admission-control-seq-gate-operation`

**Use cases**: `cpt-cf-admission-control-usecase-gate-operation`

**Actors**: `cpt-cf-admission-control-actor-enforcing-gear`, `cpt-cf-admission-control-actor-policy-engine`

```mermaid
sequenceDiagram
    participant G as Enforcing gear
    participant A as Admission service
    participant M as Built-in evaluator
    participant E as Engine client
    participant P as Policy engine
    participant R as Recorder
    G ->> A: admit(context, request)
    A ->> A: subject from context, refuse on mismatch
    A ->> M: evaluate built-ins(request)
    M -->> A: no prohibition
    A ->> E: evaluate(context, request)
    E ->> P: evaluate
    P -->> E: permission + cause
    E -->> A: EngineResult
    A ->> A: build verdict (admitted)
    A ->> R: enqueue record
    A -->> G: admitted
```

**Description**: The common path. The subject is taken from the propagated context before anything else runs, so a request asserting a different identity is refused without a built-in policy being evaluated or an engine request being built. Built-in evaluation is in-process over compiled content and bounded in time, the engine call is the only wait, and the record is enqueued rather than written so that durability does not extend the measured overhead.

#### Refuse by built-in policy

**ID**: `cpt-cf-admission-control-seq-builtin-refusal`

**Use cases**: `cpt-cf-admission-control-usecase-gate-operation`

**Actors**: `cpt-cf-admission-control-actor-enforcing-gear`, `cpt-cf-admission-control-actor-platform-operator`

```mermaid
sequenceDiagram
    participant G as Enforcing gear
    participant A as Admission service
    participant M as Built-in evaluator
    participant R as Recorder
    G ->> A: admit(context, request)
    A ->> M: evaluate built-ins(request)
    M -->> A: policy B7 prohibits
    A ->> A: build verdict (refused, built-in policy B7)
    A ->> R: enqueue record naming B7
    A -->> G: refused, built-in policy
```

**Description**: The engine is never called. The policy's identity travels to the caller and into the record, because a refusal an operator cannot attribute to a specific built-in policy is one they cannot change.

#### Refuse because the engine could not answer

**ID**: `cpt-cf-admission-control-seq-engine-failure`

**Use cases**: `cpt-cf-admission-control-usecase-gate-operation`

**Actors**: `cpt-cf-admission-control-actor-enforcing-gear`, `cpt-cf-admission-control-actor-policy-engine`

```mermaid
sequenceDiagram
    participant G as Enforcing gear
    participant A as Admission service
    participant E as Engine client
    participant P as Policy engine
    participant R as Recorder
    G ->> A: admit(context, request)
    A ->> E: evaluate(context, request)
    E ->> P: evaluate
    P --x E: timeout at configured bound
    E -->> A: could-not-run
    A ->> A: build verdict (refused, could not run)
    A ->> R: enqueue record with failure condition
    A -->> G: refused, could not run
```

**Description**: The same shape covers an unreachable engine, an error, an unmappable result, an absent engine, and an active back-off window. The caller can distinguish this from a policy refusal and retry it as transient, which is what `cpt-cf-admission-control-fr-refusal-cause` exists to make possible. A deferral does **not** take this shape: it is a result the engine returned, so it is mapped rather than funnelled, and it refuses with the awaiting-approval cause — a refusal no retry resolves, and therefore the one it matters most not to label transient.

#### Refuse because the decision could not be recorded

**ID**: `cpt-cf-admission-control-seq-record-path-failure`

**Use cases**: `cpt-cf-admission-control-usecase-gate-operation`

**Actors**: `cpt-cf-admission-control-actor-platform-operator`, `cpt-cf-admission-control-actor-audit-sink`

```mermaid
sequenceDiagram
    participant G as Enforcing gear
    participant A as Admission service
    participant R as Recorder
    participant K as event-broker audit topic
    R ->> K: publish buffered records
    K --x R: sink unreachable
    R ->> R: oldest buffered record ages past the window
    R -->> A: record path failing
    G ->> A: admit(context, request)
    A ->> A: build verdict (refused, could not run)
    A ->> R: count, do not enqueue
    A -->> G: refused, could not run
    R ->> K: drain recovers, publish one gap record
```

**Description**: The gate holds no store, so the topic is where a record becomes durable and an unreachable sink is a serving condition rather than a logging problem. Refusing is what keeps `cpt-cf-admission-control-nfr-record-completeness` true instead of quietly redefining it, and the refusals produced here are counted rather than buffered because the buffer is precisely what is exhausted. Built-in policies keep applying throughout — a refusal one of them reaches is unaffected by the state of the record path — but nothing is enqueued while the condition holds, so those refusals are counted too and are named separately in the gap record. That single record on recovery is the difference between a stated hole in the audit stream and a thin patch nobody can account for.

#### Gate a multi-type change

**ID**: `cpt-cf-admission-control-seq-gate-batch`

**Use cases**: `cpt-cf-admission-control-usecase-gate-batch`

**Actors**: `cpt-cf-admission-control-actor-enforcing-gear`

```mermaid
sequenceDiagram
    participant G as Enforcing gear
    participant A as Admission service
    participant M as Built-in evaluator
    participant E as Engine client
    participant B as Batch combiner
    G ->> A: admit_batch(context, members)
    A ->> A: check batch bound
    loop per member
        A ->> M: evaluate built-ins
        A ->> E: evaluate where no built-in prohibited
    end
    A ->> B: fold member verdicts
    B -->> A: refused, members 2 and 5
    A -->> G: batch refused, every refused member named
```

**Description**: Built-in evaluation runs per member, and only members it does not prohibit reach the engine. It is not free — a batch multiplies built-in evaluation cost by its member count, which is why the total bound of `cpt-cf-admission-control-fr-builtin-evaluation-bounds` is applied per member rather than per batch and why the batch bound caps the whole. The fold does not short-circuit, since the caller needs the complete fault list.

#### Substitute the engine

**ID**: `cpt-cf-admission-control-seq-substitute-engine`

**Use cases**: `cpt-cf-admission-control-usecase-substitute-engine`

**Actors**: `cpt-cf-admission-control-actor-platform-operator`, `cpt-cf-admission-control-actor-types-registry`

```mermaid
sequenceDiagram
    participant O as Platform operator
    participant C as Config validator
    participant T as types-registry
    participant E as Engine client
    O ->> C: start with new engine identifier
    C ->> T: resolve identifier
    T -->> C: instance
    C ->> E: hand over validated handle
    E -->> O: ready
```

**Description**: Substitution is a restart with a different identifier. Built-in policies are compiled from configuration, through the evaluation facility and the registry, with no reference to the engine at all, so they are unaffected — which is the mechanism behind `cpt-cf-admission-control-fr-builtin-policy-independence`. An identifier that does not resolve fails startup rather than yielding a gate with no engine.

### 3.7 Database schemas & tables

- [ ] `p1` - **ID**: `cpt-cf-admission-control-db-none`

**The gear owns no database objects.** It declares no schema, contributes no migrations, and holds no persistent state. Built-in policies live in deployment configuration and are compiled at startup; admission records are published to the `event-broker` audit topic rather than stored here, and the storage backend bound to that topic owns their retention and deletion; the engine owns everything about policy content.

This is stated rather than omitted because it is a design property with consequences, not an oversight. A gate with a table would need a namespace, migrations, a backup story, and a restore-consistency argument, and it would put a store on the path of every gated operation in the platform. The absence is what makes `cpt-cf-admission-control-nfr-overhead` reachable and `cpt-cf-admission-control-constraint-no-builtin-policy-api` enforceable — there is nowhere for a runtime-authored built-in policy to be written even if someone added the endpoint.

### 3.8 Deployment Topology

- [ ] `p1` - **ID**: `cpt-cf-admission-control-topology-in-process`

Two shapes are supported, and the gear's contracts are identical in both because they are transport-agnostic.

**In-process**, the shape of first release: the gate is linked into a host binary alongside its enforcing gears and the selected engine, reached through ClientHub. Every instance holds its own built-in policy set and engine handle, built independently from the same configuration, so instances need no coordination and the gate adds no cross-instance traffic. Readiness is reported as degraded rather than unready when the engine is missing, because the host is shared with the gears being gated.

**Out-of-process**, required at `p3` by `cpt-cf-admission-control-fr-remote-decision-surface`: the gate runs as its own service over the platform transport, becoming independently deployable, scalable, separately securable, and independently measurable — which is the shape the availability threshold's out-of-process clause is written for. The design admits it without change because the admission client is a trait over a service that holds no in-process assumption; what a remote deployment adds is a transport binding, not a second implementation.

Two costs come with it, and both are why it is `p3` rather than `p1`. Every gated operation crosses the network twice over, once into the gate and once onward to the engine if that is also remote, and no budget here or in the consuming gear accounts for either. And the readiness semantics invert: unreadiness is unsafe in-process, where it would evict the gears being gated, and safe once the gate is separately deployed. A design that picked one would be wrong in the other shape, so this one states both and ties each to the tier that requires it.

## 4. Additional context

### 4.1 Telemetry surface

| Signal | Type | Owner | Purpose |
|---|---|---|---|
| Admission latency excluding engine time | Histogram | Admission service | The measured surface of `cpt-cf-admission-control-nfr-overhead`, attributable to the gate rather than to the engine |
| Engine call latency and failures by condition | Histogram, counter | Engine client | Separates a slow gate from a slow engine, and an engine outage from a tightened policy |
| Verdicts by cause | Counter | Verdict builder | Distinguishes built-in-policy refusals, policy refusals, and could-not-run refusals in aggregate |
| Ungoverned share of admissions | Counter | Recorder | How much of the estate policy is silent about; this gear sees every gated operation, so the figure is complete here and nowhere else |
| Back-off windows entered and their duration | Counter, gauge | Engine client | Makes engine-requested throttling visible rather than appearing as unexplained refusals |
| Record buffer age and occupancy | Gauge | Recorder | Makes the bounded loss window observable, and is the control input for the serving condition rather than telemetry alone |
| Refusals produced while the record path was failing, and gap records published | Counter | Recorder | These refusals are deliberately not recorded individually, so the counter is the only per-decision account of them; the gap-record counter is what an auditor reconciles a thin interval against |
| Identity-mismatch refusals | Counter | Verdict builder | A rising count is a defect in a calling gear or an attempt, and either way it is neither an outage nor a policy change |
| Built-in policies loaded, and matches per policy | Gauge, counter | Built-in policy set, built-in evaluator | A built-in policy with zero matches over a long window is either dead configuration or a guardrail nobody is testing. Also readable per policy on the operational surface, since the operator asking the question usually wants it for one policy rather than as a time series |
| Built-in evaluation duration and bound exceedances, per policy | Histogram, counter | Built-in evaluator | Built-in evaluation is the dominant term in `cpt-cf-admission-control-nfr-overhead`, so the gate's own overhead has to be attributable to the policy that spent it; exceedances are separated because they present as could-not-run refusals rather than as errors |

### 4.2 Security boundaries and threat model

**Trust boundaries.** Three. The enforcing gear and its `SecurityContext` are trusted — the gate forwards identity rather than establishing it. Built-in policy content is operator-supplied and therefore more trusted than a tenant's, but it is still content the gate executes rather than data it reads, so the evaluation backend that runs it is a boundary regardless of who wrote it. The caller-supplied operation context is untrusted input, judged by built-in policies, forwarded to the engine, and never interpreted by the gate itself.

| Threat | Vector | Mitigation |
|---|---|---|
| Enforcement bypass through a failure path | A new error branch defaults to admit | Single refusal constructor; admission unreachable from any error path |
| Subject spoofing by a calling gear | A caller submits a request naming a subject, or a subject tenant, that its own context does not carry — obtaining a verdict for an identity that never asked, leaving an admission record that preserves the fiction, and choosing a tenant that widens what the engine's reachability check allows | `cpt-cf-admission-control-fr-request-authenticity`: the context is a required parameter, the subject is taken from it, a mismatch refuses through its own arm before any policy runs, and the same context is forwarded to the engine unchanged so the gate cannot substitute an identity even by accident |
| An audit gap that reads as a quiet period | The sink fails, records are dropped to keep serving, and the stream simply thins — so the interval with no evidence is indistinguishable from an interval with nothing to record | `cpt-cf-admission-control-fr-record-path-failure`: the gate refuses rather than admitting what it cannot record, counts those refusals, and publishes one gap record naming the interval and the count when the drain recovers |
| Enforcement bypass through configuration | An operator disables the gate or sets admit-on-failure | No such setting exists in the configuration type; the absent-engine case refuses rather than passes through |
| Built-in policy silently never fires | A built-in policy names a resource type that does not resolve, or content that will not compile | Resolution and syntax validation at startup, failing startup rather than loading a dead policy; per-policy match counters expose the rest |
| Configuration as an execution surface | A built-in policy's content reaches the network or the filesystem, making deployment configuration a remote-execution vector on the path of every gated operation | `cpt-cf-admission-control-fr-builtin-evaluation-bounds`: no capability is passed into evaluation — the backend receives the request context, the content, and a gate-supplied timestamp only. Reaching outward additionally requires an I/O builtin, so the backend build's registered builtin set is part of this boundary and is audited with it |
| A guardrail that does not decide the same way twice | A built-in policy reads a clock or a random generator, so the platform's own rule is not reproducible and a refusal cannot be defended | Denylisted at startup by `cpt-cf-admission-control-component-config-validator`, over the parsed form; the gate supplies the evaluation timestamp so that content has no reason to read a clock |
| Denial of service through a built-in policy | Content whose evaluation cost is unbounded, spent on every gated operation across every calling gear | Operator-configurable per-policy and total bounds, with an exceedance refusing rather than continuing; per-policy duration exported so the cost is attributable |
| Records as a leak channel | Caller-supplied property values reach a widely readable record | The record type carries names only and has no field able to hold a value |
| A compromised engine admits everything | The selected engine is malicious or defective | Built-in policies are applied before the engine and cannot be overridden by it, which bounds what a bad engine can permit |

Network segmentation, transport security, and CORS are not applicable: the gate opens no listener and exposes no external surface.

### 4.3 Testability and test strategy

The decision sequence is testable without an engine, because the engine is a trait resolved through ClientHub. The built-in evaluator is not a pure function — it calls the evaluation facility — so the facility is injected too, and selection is separable from evaluation and tested on its own.

| Level | Coverage |
|---|---|
| Unit | Built-in policy selection including wildcard precedence, batch folding, cause construction, configuration validation |
| Integration | Full sequence against a stub engine covering permission, prohibition, back-off, and each failure condition; built-in evaluation against a real backend covering prohibition, fall-through, per-policy bound exceedance, and total bound exceedance |
| Fault injection | The failure set enumerated by `cpt-cf-admission-control-nfr-fail-closed`; this is that requirement's named verification method. Includes a per-policy bound exceedance, a total bound exceedance, and a backend failure during built-in evaluation as distinct conditions — the only three in the set that arise in this gear's own code, and the ones the stall clause of `cpt-cf-admission-control-nfr-availability` is measured against, since a cooperative bound is what makes a stall conceivable at all. Includes the audit sink unreachable and the record buffer saturated as distinct conditions, each asserting refusal rather than admission, no record dropped or sampled, and exactly one gap record once the drain recovers |
| Security | An authenticity test asserting that a request whose asserted subject or subject tenant disagrees with the propagated context refuses with its own cause, that no built-in policy is evaluated and no engine request is constructed, and that the record names the context's identity — run against an authenticated deployment and against an auth-disabled one whose context carries the platform default subject, so the check cannot become a dev-mode outage; a determinism test admitting the same request twice and asserting identical verdicts; a test enumerating the backend build's registered builtins that fails when one is absent from the denylist, so an upgrade adding a non-deterministic builtin breaks the build rather than a guardrail |
| Conformance | Request-unchanged assertion on both verdicts, which is the measurable form of `cpt-cf-admission-control-nfr-non-modification` |
| Performance | Gate-boundary latency with engine time subtracted, separating selection from evaluation; built-in evaluation cost against set size and against content complexity; achieved versus configured per-policy bound, since a cooperative check overruns and `cpt-cf-admission-control-nfr-overhead` is measured on what happened |
| Contract | The engine plugin trait exercised through a second stub implementation, which is also the test that `cpt-cf-admission-control-usecase-substitute-engine` describes; includes a stub returning a deferral, asserting the awaiting-approval cause rather than could-not-run, and an assertion that the context the stub receives is the one the caller presented |
| Conformance, built-in independence | The reserved-name-prefix fixture of `cpt-cf-admission-control-fr-builtin-policy-independence` run against two stub engines and against a deployment with no engine, asserting the same refusal in all three; this is what keeps the property falsifiable while the shipped built-in set is still an open question |

Compile-fail coverage applies to one invariant: the verdict builder's exclusivity. If another module can construct a Verdict, the fail-closed property is convention rather than structure, and a trybuild case asserting that it cannot is the only way to keep that true as the gear grows.

### 4.4 Known design risks

| Risk | Consequence | Response |
|---|---|---|
| A backend build registers a non-deterministic builtin the denylist does not name | A platform guardrail decides differently on two identical requests. Built-in refusals are the ones an operator trusts most, so the inconsistency is attributed anywhere but to the rule | Bind the denylist to the audited build; enumerate the backend's registered builtins in a test rather than maintaining the list by hand; re-audit on upgrade. One audit serves this gear and the policy engine |
| The cooperative evaluation bound overruns the gate's overhead budget | `nfr-overhead` is measured at p95 5 ms with built-in evaluation as its dominant term, and a bound checked between work units can overshoot by a whole unit | Set the check interval explicitly rather than inheriting it, keep the per-policy bound well inside the total, and measure the achieved figure rather than asserting the configured one |
| Built-in evaluation cost exceeds its share of the borrowed budget | The gate's own overhead breaches `cpt-cf-admission-control-nfr-overhead` even while the engine is fast, and the breach is attributed to the gear rather than to the configuration that caused it | Keep the built-in set small, hold the per-policy bound well inside the total, and export duration per policy so the cost names its own source |
| The built-in set grows until it needs a lifecycle | Versioning, review, and tenant scoping are wanted for built-in policies, and the gate becomes a second policy engine with a second content model | The boundary is management, not expressiveness: content may say anything the facility can express, and arrives only as deployment configuration. Treat a request to author one at runtime as a request for content in the engine |
| Fail-closed makes the engine a platform-wide single point of failure | Every engine incident stops all gated operations, and pressure builds for a bypass | Keep the engine bound tight so an outage is detected quickly; keep the could-not-run cause retryable so callers degrade rather than fail permanently |
| The gate is specified before either component it sits between | Its interface is settled against assumptions rather than a consumer, and defects surface at integration | Exercise the interface against Infrastructure Resource Manager's stated admission requirements and against the policy engine's decision surface, both of which are specified even though neither is built |
| A new engine outcome falls into the failure funnel | The design converges every unmapped result on the could-not-run constructor, which is what makes fail-closed structural — and also what would silently report a deferral, or any later outcome, as an outage | Name every outcome in the engine result type before an engine emits it, deferral included from the first version; assert in the contract test that a deferral produces the awaiting-approval cause and not the could-not-run one |
| Obligations are dropped or reordered in the relay | A conditional permission becomes an unconditional one, silently, and the enforcing gear proceeds without the condition policy attached | Copy obligations by the same move that copies the permission cause, so no branch exists that could inspect or discard one; assert relay fidelity in the contract test against a stub engine that emits them |
| Record volume equals total gated operation volume | The audit topic becomes the platform's highest-volume stream — carrying this gear's records and the engine's — and buffer pressure appears as refusals under the serving condition | Keep the record projection small — names not values, no request copy — and export buffer age as a first-class signal rather than as a debug metric |

### 4.5 Areas recorded as not applicable

Stated so that absence is distinguishable from oversight. **Data storage, migrations, and backup**: the gear owns none, per Section 3.7. **Caching**: nothing on the decision path is cached, because nothing on it is fetched. **Rate limiting**: the gate performs no work proportional to request size and is bounded by the engine behind it; the platform ingress owns inbound limiting. **Infrastructure-as-code and release strategy**: host concerns, and a mixed-version fleet is safe because each instance compiles its own built-in policy set from the same configuration. **An external decision API**: not offered in the in-process shape, where the decision surface is a typed client; the out-of-process shape projects it over the platform transport, and the latency consequence is recorded as an open question rather than absorbed silently. **Event architecture**: the gate publishes admission records to the `event-broker` audit topic and nothing else — no domain events announcing its own state changes, and no subscriptions. **Advisory output**: the gate relays obligations, which arise from the policy evaluation it mediates, and carries no warning channel — consumption diagnostics and threshold crossings come from the quota path, which an enforcing gear calls itself and which this gate does not sit on. **User-experience architecture**: no end-user interface. **Cost budgets**: not modelled per gear in this repository.

## 5. Traceability

- **PRD**: [PRD.md](./PRD.md)
- **ADRs**: `ADR/` — not yet created; the decisions listed in Section 1.2 land here once written
- **Features**: `features/` — not yet created
- **Policy engine**: [Policy Engine](../../policy-engine/docs/PRD.md) and its [DESIGN](../../policy-engine/docs/DESIGN.md), the first implementation of the plugin contract
- **First enforcing gear**: [Infrastructure Resource Manager](../../../infrastructure-resource-manager/docs/PRD.md)
- **Platform architecture**: [ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md)
