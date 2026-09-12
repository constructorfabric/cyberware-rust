# PRD — Policy Engine

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Gear-Specific Environment Constraints](#31-gear-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Policy Content Model](#51-policy-content-model)
  - [5.2 Bundle Lifecycle](#52-bundle-lifecycle)
  - [5.3 Assignment and Inheritance](#53-assignment-and-inheritance)
  - [5.4 Policy Matching](#54-policy-matching)
  - [5.5 Decision Semantics](#55-decision-semantics)
  - [5.6 Evaluation Inputs and Outputs](#56-evaluation-inputs-and-outputs)
  - [5.7 Expression Evaluation](#57-expression-evaluation)
  - [5.8 Multi-Tenancy](#58-multi-tenancy)
  - [5.9 Observability](#59-observability)
  - [5.10 Configuration](#510-configuration)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Efficiency](#61-efficiency)
  - [6.2 Reliability](#62-reliability)
  - [6.3 Performance](#63-performance)
  - [6.4 Security](#64-security)
  - [6.5 Versatility](#65-versatility)
  - [6.6 NFR Exclusions](#66-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

The Policy Engine is the Gears platform gear that owns admission control policy: it stores policy content, manages its lifecycle, evaluates it against the operations that management gears are about to perform, and returns a decision the caller enforces. It is the platform's home for policy that no single gear owns — content that is authored, versioned, assigned to tenants, and audited independently of the gears whose operations it governs.

The gear is one of two components. The other is `admission-control`, a system gateway that admits or rejects an operation and delegates the check to a policy engine, of which this gear is one implementation — selected the way this platform selects every plugin, one at a time. This document specifies the engine only; the gateway, including how it is invoked and what it does with the answer, is specified separately. The gateway is this gear's only consumer: management gears reach policy through it, not around it.

### 1.2 Background / Problem Statement

Management gears need to refuse operations that violate a tenant's rules before those operations take effect, and today each one solves that alone. [Infrastructure Resource Manager](../../../infrastructure-resource-manager/docs/PRD.md) records the consequence directly: policy enforcement across the estate was inconsistent and left audit gaps, which is why its requirements now demand that every operation be policy-gated. It declares a Policy Decision Service actor for that purpose and states that the actor is a capability rather than a component.

That actor is not unserved. Infrastructure Resource Manager maps it, informatively, onto the platform of today: admission and policy decisions to `authz-resolver`, quota to `quota-enforcement`, licence to the licence resolver — and it records the resulting dependency status as partial. What is missing is narrower and more specific than "no provider". No component owns tenant-authored policy content that spans gears: content with a lifecycle, a version history, an assignment to a tenant subtree, and an audit trail, judged against operations that gears are about to perform.

[`quota-enforcement`](../../quota-enforcement/docs/PRD.md) and [`event-broker`](../../event-broker/docs/PRD.md) have each met a narrower version of this need, and neither generalises. Both evaluate policy over state they own themselves, and both keep their engine registry local to that domain. Policy that spans gears has no such owner, and Section 5.7 explains why this gear does not build a third local registry.

Admission control policy has no owner today. The rules that say which operations a tenant may perform, under which conditions, are not the property of the gear that happens to execute them — they belong to whoever administers the tenant, they outlive any single gear, and they have to be reviewable as a body. Without a place to keep them, they end up compiled into the gears they constrain, where changing one requires a release and auditing one requires reading source.

### 1.3 Goals (Business Outcomes)

Milestones below refer to this document's own priority tiers — "p1 complete" means every `p1` requirement in Sections 5 and 6 is met and verified. The repository has no separate release-maturity vocabulary.

| Outcome | Baseline | Target | By |
|---|---|---|---|
| A management gear can gate its operations on tenant policy without embedding rules in its own source | Zero shared decision paths; each gear hard-codes its checks | One supported decision path, exercised against the admission requirements Infrastructure Resource Manager specifies, through a harness standing in for the gateway until that gear exists | p1 complete |
| Policy changes are versioned, attributable, and reviewable | Governance rules live in gear source; changing one requires a release | 100 percent of policy changes carry an author, a version, and a content digest, and take effect without a deployment | p1 complete |
| An administrator can see what policy currently refuses | No record of refusals beyond gear-local logs | Every refusal is retrievable by tenant, by policy, and by resource type within the retention window | p1 complete |
| Decisions are reconstructable after the fact | No decision records | Any decision in the retention window can be traced to the subject, the operation, and the policy version that determined it | p1 complete |
| An operator can see how much of the estate policy is silent about | No record of what is ungoverned; absence is invisible | Every permission carries its cause, and the ungoverned share of decisions is observable as a metric | p1 complete |

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Admission | The act of permitting or refusing an operation before it takes effect. |
| Admission Control Policy | A rule that judges whether a management operation on a resource may proceed, evaluated against the operation's context. The content this gear owns and evaluates; the `admission-control` gear enforces the verdict but does not hold the content. |
| Authorization Policy | A rule that judges whether a subject may act on a resource at all. A distinct kind, owned by `authz-resolver` and specified in [arch/authorization/](../../../../docs/arch/authorization/). This gear neither stores nor evaluates it, and the two kinds share no content, no lifecycle, and no decision path. |
| Entitlement | What a subject is permitted to do, as recorded by the authorization subsystem: a role, a role binding, a permission or scope, a group membership, or the resolved statement that a subject may perform an action. Owned by `authz-resolver`. Distinct from the subject's **identity** — an identifier and a tenant — which is all this gear's evaluation input carries, per `cpt-cf-policy-engine-fr-authorization-boundary`. |
| Admission Gateway | The `admission-control` system gear: a thin gateway that admits or rejects, delegating the check to the one policy engine selected in that deployment. Specified separately. |
| Policy Bundle | A versioned, deployable collection of policy documents, assigned to a tenant. |
| Policy Document | A single unit of policy content within a bundle, carrying a kind that determines how it is evaluated. |
| Policy Target | The binding that determines when a policy document is evaluated, by trigger type, phase, resource type, and filters. |
| Decision | The result of evaluating policy for a subject, an action, and a resource: permit or prohibit. Distinct from a document **outcome**, which is what one policy document yields. |
| Ungoverned | The cause on a permission produced when no document permitted or prohibited. The operation proceeds, and the record says policy was silent about it rather than that policy approved it. |
| Decision Record | The durable record of one evaluation, carrying the field set defined in Section 5.9. |
| Refusal | A prohibiting decision. Exposed as a queryable projection over decision records within the retention window, not as separately stored state, and therefore a history of what policy refused rather than a list of conditions currently in breach. |
| Obligation | An action a caller is required to perform when enforcing a permitting decision. |
| Assignment | The binding of an active bundle to a tenant, determining which resources the bundle governs. |
| Policy Priority | The ordering value on an assignment, used to break ties between assignments at the same tenant. |
| Evaluation Facility | The platform's shared policy-evaluation toolkit library. It carries the contract that evaluation backends implement and selects among them, so a consumer links one facility rather than its own engine registry. Specified separately; this gear neither defines it nor chooses which backends it ships. |
| Evaluation Backend | An implementation of a policy language behind the evaluation facility's contract. Policy content is written in the language of one backend, and declares which. |
| PDP / PEP / PAP | Policy Decision Point, Policy Enforcement Point, Policy Administration Point. This gear administers and evaluates admission control policy, which makes it a PAP and a PDP for that content. The gateway that calls it is a decision point too, not an enforcement point: the PEP is the enforcing gear behind the gateway, because that is the only component able to stop the operation. The vocabulary is the same role pattern the authorization subsystem uses, applied to a different content kind — it does not imply a shared decision path. |
| GTS | Global Type System. Provides the identifiers used for resource types, plugin instances, and error codes. |

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level by steering committee. Documented below are the actors that interact with this gear.

### 2.1 Human Actors

#### Policy Author

**ID**: `cpt-cf-policy-engine-actor-policy-author`

- **Role**: Authors and revises policy content, submits it for validation, and promotes it through the bundle lifecycle.
- **Needs**: Authoring and validation feedback before activation; deterministic evaluation semantics; the ability to see what a policy will refuse before it takes effect.

#### Platform Operator

**ID**: `cpt-cf-policy-engine-actor-platform-operator`

- **Role**: Configures and operates the gear, sets limits, and responds to availability and latency incidents.
- **Needs**: Configuration with safe defaults; visibility into decision latency, cache behaviour, and refusal volume; predictable failure semantics.

#### Tenant Policy Administrator

**ID**: `cpt-cf-policy-engine-actor-tenant-policy-admin`

- **Role**: Manages policy for a tenant subtree within the bounds set by ancestor tenants, and reviews what that policy currently refuses.
- **Needs**: Policy management confined to their own subtree; visibility into which inherited policies constrain them; a list of current violations they can act on.

#### Security Auditor

**ID**: `cpt-cf-policy-engine-actor-security-auditor`

- **Role**: Reviews admission decisions after the fact for compliance and incident investigation.
- **Needs**: A complete decision record with enough context to reconstruct why a decision was reached, including the policy version in force at the time.

### 2.2 System Actors

#### Admission Gateway

**ID**: `cpt-cf-policy-engine-actor-admission-gateway`

- **Role**: The `admission-control` system gear, and this gear's only direct consumer. Selects this gear as its policy engine, supplies the operation context on behalf of the gear being gated, and relays the resulting decision back to it. Every evaluation this gear performs arrives through the gateway.

#### Enforcing Gear

**ID**: `cpt-cf-policy-engine-actor-enforcing-gear`

- **Role**: A management gear whose operations are gated on policy — Infrastructure Resource Manager first among them, through the admission requirements in its own PRD. It originates the operation context and enforces the verdict, but it reaches this gear through the gateway and never calls it directly. Its requirements shape what policy must be able to express; its integration is with `admission-control`.

#### Hierarchy Provider

**ID**: `cpt-cf-policy-engine-actor-hierarchy-provider`

- **Role**: `tenant-resolver` supplies tenant ancestry, descendants, and barrier state. Acts as a Policy Information Point during assignment resolution.

#### Types Registry

**ID**: `cpt-cf-policy-engine-actor-types-registry`

- **Role**: Receives this gear's GTS registrations, and resolves the four kinds of identifier this gear stores or emits but does not own: the evaluation backend a document declares, the concrete resource types a target names, the obligation identifiers a decision carries, and the plugin instances behind them. Every one of those is content the gear validates rather than interprets, which is why the registry is on the authoring path as well as the evaluation path.

#### Decision Record Sink

**ID**: `cpt-cf-policy-engine-actor-audit-sink`

- **Role**: Receives the decision records this gear emits, for retention, export, and analysis. The sink is `event-broker`, which carries an audit-pipeline use case for exactly this shape: records are published to a platform-scoped topic, and the storage backend bound to that topic owns their retention, compaction, and deletion. The `admission-control` gateway publishes its admission records to the same topic, so the record of what was gated and the record of how policy decided it are read from one stream rather than joined across two systems.

## 3. Operational Concept & Environment

> Project-wide runtime, operating system, architecture, lifecycle policy, and integration patterns are defined at the repository level in [ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md) and the foundational [guidelines/](../../../../guidelines/). This gear has no parent gear PRD. Only gear-specific constraints appear below.

### 3.1 Gear-Specific Environment Constraints

- The gear sits inside the admission path of its consumers. Every gated operation results in an evaluation, so the gear's availability and latency bound the availability and latency of the management operations it governs, and it fails closed by design.
- The gear is a decision authority, not a data owner for the resources policy governs. It never holds copies of those resources and never requires a consumer to synchronise resource state into it. Everything an evaluation needs arrives on the request.
- The gear holds two data sets and owns neither outright. Policy content belongs to the tenant it is assigned to, which authors and withdraws it; the gear is its custodian and versioning authority. Decision records belong to the platform's audit function; the gear produces them and applies retention, and consumers of the record stream are entitled to them independently of this gear.
- First release composes the gear in-process, the platform's default execution mode: the gear, its consumers, and the evaluation facility share one runtime and communicate through typed clients. The platform also supports out-of-process execution over gRPC, and the gear's contracts are unchanged by that choice, but two requirements are conditioned on it — `cpt-cf-policy-engine-nfr-availability` explicitly, and `cpt-cf-policy-engine-nfr-decision-latency` implicitly, since its budget contains no allowance for a transport hop.
- The gear depends on a platform evaluation facility that does not yet exist: no such library, gear, or specification is present in the repository today. It is a prerequisite rather than a dependency the gear can degrade around — without it there is no evaluation path at all, and the backends it eventually carries bound which languages policy content can be written in. Every requirement below that refers to the facility is conditional on its arrival.
- [GEARS.md](../../../../docs/GEARS.md) has no entry for this capability. Its nearest neighbour, Policy Manager, is defined as managing *authorization* policies for resources and actions, which Section 1.4 places outside this gear and which `authz-resolver` serves. The catalog needs a new entry rather than a reinterpretation of that one.
- The gear's placement under `gears/system/` is provisional. A separation of core system gears from infrastructure-management gears has been proposed, and this gear would belong with the latter, but the repository defines three tiers today and no such grouping exists. Nothing in this document depends on where the gear finally sits.

## 4. Scope

### 4.1 In Scope

- Policy content model: bundles, versions, documents, targets, and filters, with their relationships and constraints.
- Bundle lifecycle: draft, activation, deprecation, immutability after activation, content integrity, and optimistic concurrency.
- Policy assignment to tenants, with inheritance, precedence, and barrier handling across the platform's tenant tree.
- Evaluation of a subject, an action, and a resource against the applicable policy set, returning a single decision with a reason.
- Trigger-based matching of policy documents to operations by type, phase, resource type, and attribute filters.
- Batch evaluation, returning one atomic verdict over the several resource types a single consumer change touches.
- Obligations attached to permitting decisions.
- Decision records covering every evaluation, and a violations projection over them.
- Multi-tenant isolation of policy content and decisions.
- A management API for policy content, decisions, and violations, offered as both an in-process client and a REST surface.
- Configuration, observability, and operational limits.

### 4.2 Out of Scope

- The `admission-control` gateway gear: how it is invoked, how it selects an engine, what it does with a decision, and what its built-in platform policies are, are specified separately.
- The shared evaluation facility: which backends it carries, the languages they accept, their evaluation semantics, and its own requirements belong to its own specification. This gear depends on it, declares which language its content is written in, and defines none of it.
- Generating and mutating policies, in the sense that Kyverno uses those terms — policy that creates or rewrites resources rather than judging them. Explicitly excluded at every priority tier, not deferred to one.
- Authorization policy, in the sense defined in Section 1.4: rules about whether a subject may act on a resource at all. That is a distinct content kind owned by `authz-resolver`, and this gear neither stores nor evaluates it. `cpt-cf-policy-engine-fr-authorization-boundary` makes the exclusion enforceable rather than declared, by withholding from the evaluation input the entitlements such a rule would need. Should the gear ever be asked to evaluate authorization policy, that is a new capability requiring its own requirements, and the positions recorded in [arch/authorization/](../../../../docs/arch/authorization/) would govern it.
- Row-level constraint generation for collection queries, and the capability negotiation it requires. Constraints are an artifact of the authorization evaluation model, so they belong to that subsystem rather than to a later phase of this gear.
- The identity of subjects and the validation of their credentials, which belong to `authn-resolver`.
- Enforcement itself. Consumers enforce the decisions they receive; this gear has no mechanism to make them.
- Tenant hierarchy ownership, which belongs to `tenant-resolver`. This gear reads hierarchy and does not maintain it.
- Allowance state and its consumption, which belong to `quota-enforcement`. This gear does not hold counters, reserve allowance, or track consumption.
- A policy authoring user interface.
- Rolling a version out to a proportion of requests. Staged activation is expressed through assignment scope, effective window, and the enforcing flag — all of which are properties of *which* operations an assignment governs. Sampling by request fraction would make two evaluations of the same subject differ, which `cpt-cf-policy-engine-fr-deterministic-ordering` and the reproducibility of decision records both rule out.
- Migration of policy content from external policy managers.

## 5. Functional Requirements

> **Testing strategy**: All requirements are verified via automated tests targeting 90 percent or greater code coverage unless otherwise specified. Verification method is documented only where a non-test approach applies.

### 5.1 Policy Content Model

#### Bundle Composition

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-bundle-composition`

The system **MUST** represent policy content as bundles that contain policy documents, where each document carries a kind that determines how it is evaluated, a GTS identifier naming the evaluation backend its content is written for, and one or more targets that determine when it is evaluated. A bundle is the unit of versioning, assignment, and integrity.

Document content is opaque to this gear: it is source text the declared backend interprets, not a structure the gear parses. The gear resolves the declared backend identifier through the types registry, routes the document to the instance that identifier names, and interprets only the outcome that comes back. Typing the backend rather than naming it by convention is what lets a deployment carry more than one and lets validation reject content no backend can run.

- **Rationale**: Policy versioned per document cannot be reasoned about as a whole, and policy versioned per tenant cannot be shared. The bundle is the granularity at which authors think about a coherent set of rules and at which operators activate and withdraw them. The language declaration is what lets the evaluation facility carry more than one backend without the gear guessing: content that does not say what it is written in can only be routed by convention, and a convention is what breaks when a second backend arrives.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-types-registry`

#### Document Kinds

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-document-kinds`

The system **MUST** distinguish policy documents by kind and **MUST** route each kind to the evaluation path appropriate to it. The kind set is closed and versioned, and at first release has one member: guardrail, which judges whether an operation is permitted. Adding a kind is a versioned change to the set, not a configuration option.

- **Rationale**: A closed set lets consumers request only the policy relevant to them and lets the gear apply the right combination semantics, and declaring the discipline now is what makes a second kind a versioned change rather than a reinterpretation. One member is the honest count: every kind previously sketched here had no consumer asking for it, and a kind whose content the gear does not interpret cannot produce an outcome `cpt-cf-policy-engine-fr-responsibility-boundary` is able to map to the closed set.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-enforcing-gear`

#### Target Binding

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-target-binding`

The system **MUST** allow each policy document to declare when it applies, by trigger type, evaluation phase, resource type, and attribute filters. Filters combine conjunctively. Resource type matching **MUST** support exact type identifiers and type patterns.

Attribute filters compare a named attribute of the caller-supplied operation context against a literal value, using a closed operator set: equal, not equal, member of, not member of, present, and not present. Comparison is type-sensitive: an attribute whose supplied type differs from the literal's does not match, and **MUST NOT** be coerced. An attribute the caller did not supply is absent, and absent matches only the not-present operator — so a filter written over an attribute a caller omits stops selecting its document rather than selecting it by default. A target **MUST** be able to declare an attribute required, in which case an evaluation that omits it is an infrastructure failure rather than a silent non-match. Resource type patterns are the pattern forms the platform's type system already defines — a concrete identifier and a wildcard over a namespace — so this gear introduces no pattern syntax of its own. A concrete identifier **MUST** resolve through the types registry, and content naming one that does not **MUST** fail validation rather than activating as a target that can never match.

The trigger and phase vocabularies are likewise closed and versioned. Trigger types are: an operation on a resource, and a system or domain event. Phases are: before the operation, where the outcome can refuse it; and after the operation, where it cannot. Adding to either set is a versioned change. First release implements the operation trigger in the before phase; the event trigger and the after phase are declared so that targets authored now remain valid when they land, and their implementation is tracked separately.

- **Rationale**: Evaluating every document on every operation does not scale and makes policy behaviour opaque. Declared targets let the gear compute a small applicable set and let authors state intent precisely. The vocabularies are enumerated here because other requirements match on them, and a matching rule over an undefined set cannot be verified. Resolving concrete type identifiers against the registry is what keeps a typo from becoming a silently inert guardrail — the failure mode an author is least able to detect, because nothing happens.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-types-registry`

#### Content Validation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-content-validation`

The system **MUST** validate policy content before it can be activated, and **MUST** report validation failures against the specific documents that caused them without activating any part of the bundle. Validation **MUST** cover content syntax as the declared backend judges it, resolution of the declared backend identifier through the types registry to an instance the deployment carries, resolution of every concrete resource-type identifier the document targets, the declared target vocabularies, the absence of every builtin denylisted for that backend build by `cpt-cf-policy-engine-fr-evaluation-isolation`, and the operational limits in Section 5.10.

- **Rationale**: Invalid content discovered at evaluation time fails closed and refuses real operations. Validation at authoring time moves that failure to a person who can fix it. Because the backend that will evaluate the content is already available to parse it, syntax validation costs an authoring-time call rather than new machinery, which is why this is p1 rather than deferred. Checking backend availability belongs here too: content naming a backend the deployment does not carry is undeployable, and discovering that at activation is far cheaper than discovering it on the first gated operation.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-types-registry`

### 5.2 Bundle Lifecycle

#### Lifecycle States

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-lifecycle-states`

Lifecycle state belongs to a bundle **version**, not to the bundle: a bundle is a stable identity and a name, and its versions move through draft, active, and deprecated. The system **MUST** permit modification only while a version is draft, and **MUST** reject modification of an active or deprecated version with a distinguishable conflict result.

The system **MUST** support these transitions and no others: create a draft, either empty or seeded from any retained version of the same bundle; activate a draft, which deprecates the previously active version of that bundle; deprecate an active version; and delete a draft, which is the only deletion the lifecycle permits. A deprecated version **MUST NOT** be reactivated in place. Returning to earlier content means seeding a new draft from the retained version and activating that, and every activation — including one that restores earlier content — **MUST** produce a new version identity and **MUST** be recorded as its own administration event under `cpt-cf-policy-engine-fr-administration-audit`. At most one version of a bundle **MUST** be active at a time.

- **Rationale**: An activated bundle is a decision authority. If it can change underneath, no decision record can be reproduced and no reviewer can be sure what was in force at a given moment. Requiring a new identity and an event for every activation is what keeps restoring earlier content from becoming a quieter path than authoring it: a design that instead rewound a pointer would let a principal who may roll back but may not author widen access by reverting to a more permissive version, with no authoring check and nothing in the audit trail that looks like a grant.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-platform-operator`

#### Administration Audit

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-administration-audit`

The system **MUST** record every change to policy content, assignment, and lifecycle state as an administration event, and **MUST** retain those events for at least as long as the content versions they describe. Every event **MUST** carry the acting subject and their tenant, the time, the operation, the identity and version of the content affected, and the precondition token the caller supplied where the operation required one.

- **Rationale**: Two commitments in this document depend on attribution that no other requirement delivers: the goal that every policy change carries an author, and the criterion that every state change is attributable to an actor. Decision records cannot serve — they describe evaluations, not authorship, and `cpt-cf-policy-engine-fr-decision-records` owns a field set with no administration fields in it. Retention has to outlast the content rather than follow the decision-record window, because the question an auditor asks is who put a rule in force, and that question outlives every decision the rule produced.
- **Actors**: `cpt-cf-policy-engine-actor-security-auditor`, `cpt-cf-policy-engine-actor-tenant-policy-admin`, `cpt-cf-policy-engine-actor-platform-operator`

#### Content Integrity

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-content-integrity`

The system **MUST** record a content digest for each activated bundle version, and **MUST** verify it whenever content is loaded for evaluation and at each activation. Content whose digest does not match the recorded value **MUST NOT** be evaluated. Verification is not required per evaluation, which the decision latency budget could not absorb; the guarantee is therefore that no unverified content is ever loaded, not that every decision re-checks it.

- **Rationale**: Policy is a security control. Undetected modification, whether from storage corruption or tampering, silently changes what the platform permits.
- **Actors**: `cpt-cf-policy-engine-actor-security-auditor`, `cpt-cf-policy-engine-actor-platform-operator`

#### Optimistic Concurrency

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-optimistic-concurrency`

The system **MUST** require a precondition on every modifying operation against existing policy content and **MUST** reject the operation when the content has changed since it was read.

- **Rationale**: Multiple administrators editing one tenant's policy is normal. Last-write-wins on a security control silently discards the other author's change.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-tenant-policy-admin`

#### Version History

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-version-history`

The system **MUST** retain previous versions of a bundle after a new version is activated, **MUST** make each retained version identifiable by the decision records that reference it, and **MUST** support returning a bundle to a previously retained version by seeding a new draft from it, per the transitions in `cpt-cf-policy-engine-fr-lifecycle-states`, rather than by mutating a frozen version.

- **Rationale**: Auditing a past decision requires the content that produced it, not the content in force today. The same retained versions make recovery from a bad activation a single operation rather than a reconstruction from memory.
- **Actors**: `cpt-cf-policy-engine-actor-security-auditor`, `cpt-cf-policy-engine-actor-platform-operator`

#### Deprecation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-deprecation`

The system **MUST** allow an active bundle to be deprecated, **MUST** stop including deprecated bundles in evaluation within the activation propagation window, and **MUST** retain the bundle and its history for audit.

- **Rationale**: Withdrawing policy must be as controlled as activating it, and must not destroy the record of what was in force.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`

#### Version Comparison

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-fr-version-comparison`

The system **MUST** support comparing two retained versions of the same bundle, reporting the documents added, removed, and changed between them, and **MUST** flag a comparison in which the later version could permit something the earlier one prohibited.

The flag is deliberately conservative: it **MUST** report every change that could widen — a prohibiting document removed, an outcome changed away from prohibit, or a target narrowed so a prohibiting document no longer matches — and reporting a change that turns out not to widen in practice is acceptable rather than a defect. The system **MUST NOT** claim that an unflagged comparison cannot widen.

- **Rationale**: Reviewers and approval flows need one question answered above all others: does this change grant something the current version denies. Everything needed to answer it is already retained — immutable versions and their digests — and without a comparison surface every embedding platform invents its own diff semantics over the same store, which then disagree. Deciding widening exactly would mean comparing two decision functions over every possible input, which is not tractable for a general policy language; so the requirement is a sound over-approximation that never misses a widening and sometimes cries wolf, and it says so rather than implying a guarantee it cannot hold. Comparing against recorded decisions instead is not available: `cpt-cf-policy-engine-fr-record-confidentiality` keeps property values out of records, so a recorded decision cannot be replayed against a candidate version.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-security-auditor`

#### Effective Windows

- [ ] `p3` - **ID**: `cpt-cf-policy-engine-fr-effective-windows`

The system **MUST** allow an assignment to carry a start and an end time, and **MUST** include the assignment in evaluation only within that window. The window composes with the assignment's tenant scope and with `cpt-cf-policy-engine-fr-non-enforcing-assignment` rather than replacing either: the scope decides where an assignment applies, the window decides when, and the enforcing flag decides whether its outcome counts. All three are independent.

- **Rationale**: Time-bounded policy — a maintenance window, a temporary relaxation, a contractual period — is otherwise implemented as a manual withdrawal that someone forgets.
- **Actors**: `cpt-cf-policy-engine-actor-tenant-policy-admin`, `cpt-cf-policy-engine-actor-platform-operator`

### 5.3 Assignment and Inheritance

> Policy is assigned to tenants in the platform's single-root tenant tree and inherited down that tree. The gear introduces no hierarchy of its own.

#### Tenant Assignment

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-tenant-assignment`

The system **MUST** allow a bundle to be assigned to a tenant with an explicit policy priority, and **MUST** apply it to the tenant and to its descendants in the tenant tree. An assignment **MUST** remain in force across activations of that bundle: activating a new version **MUST** change what every existing assignment of it governs, and the system **MUST NOT** require any administrative action to re-establish an assignment after an activation. The system **MUST** equally allow an assignment to be withdrawn, and its priority, effective window, and barrier-reach declaration to be changed. Withdrawal and change **MUST** take effect within the activation propagation window and **MUST** be recorded as administration events, on the same terms as activation.

- **Rationale**: Policy that must be attached to every tenant individually is unmaintainable at any real tenant count, and drifts the moment a tenant is created. Assigning to the platform's existing tree means policy inherits the structure everything else in the platform already uses. Assignments surviving activation is what makes the recovery claim in `cpt-cf-policy-engine-fr-version-history` true: if activation left existing assignments needing repair, returning to earlier content would take one activation plus one repair per affected tenant, and a partial failure would leave some tenants governed by nothing while still appearing to be governed — reported, correctly but silently, as the ungoverned permits of `cpt-cf-policy-engine-fr-permit-provenance`. An administrator would have to notice the absence of refusals to notice the fault.
- **Actors**: `cpt-cf-policy-engine-actor-tenant-policy-admin`, `cpt-cf-policy-engine-actor-platform-operator`

#### Nearest Tenant Precedence

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-nearest-tenant`

Where assignments at more than one tenant in an ancestry chain apply to the same evaluation, the system **MUST** order them by proximity to the resource, nearest first, breaking ties among assignments on the same tenant by policy priority, highest first. This ordering **MUST NOT** affect the result, which `cpt-cf-policy-engine-fr-outcome-combination` determines independently of position. It determines which document a refusal names as responsible, and the order in which the applicable set is consumed before short-circuiting.

- **Rationale**: Delegation works through denial precedence, not through position: a tenant refines what an ancestor permitted by prohibiting, and a prohibition anywhere in the chain refuses regardless of where it sits. That makes ancestor guardrails a consequence of the combination rule rather than a separate mechanism, which is why no separate guardrail requirement appears in this document — and it also means position cannot be allowed to decide anything, or a descendant could remove a constraint its ancestor is accountable for. What proximity is for is the refusal a tenant administrator actually reads: naming the nearest responsible policy first points them at the one they can change, rather than at an ancestor's guardrail they cannot.
- **Actors**: `cpt-cf-policy-engine-actor-tenant-policy-admin`, `cpt-cf-policy-engine-actor-platform-operator`

#### Non-Enforcing Assignment

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-fr-non-enforcing-assignment`

The system **MUST** allow an assignment to be marked non-enforcing. A non-enforcing assignment **MUST** be evaluated and recorded exactly as an enforcing one, and its outcomes **MUST NOT** contribute to the result returned to the caller.

- **Rationale**: Activating a new bundle is otherwise the only way to learn what it does to real traffic, which makes every rollout a production experiment. A non-enforcing assignment measures a candidate against live requests with no risk to them, and it is a property of the assignment rather than of the content, so the same version can enforce in one tenant subtree while observing in another. This is distinct from `cpt-cf-policy-engine-fr-dry-run`, which answers a supplied hypothetical, and from the observe document outcome, which is authored into content and cannot be turned off without editing it.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-platform-operator`

#### Barrier Handling in Inheritance

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-inheritance-barriers`

The system **MUST** resolve the subtree an assignment reaches using the barrier handling supplied by the caller on the evaluation request, and **MUST NOT** apply a barrier default of its own. Where an assignment declares that it reaches through barriers, that declaration **MUST** widen the resolved subtree only for evaluations whose caller requested barrier handling that permits it, and **MUST NOT** override a caller that requested barriers be respected.

- **Rationale**: The platform's position is that barriers are context-dependent and the caller decides per resource type. A stored barrier setting that overrode the caller would reverse that. The assignment-level declaration exists only so an operator guardrail can be marked as one that may reach through when the caller already permits it; composition is caller-first, and a gear-level default would make one of the two cases silently wrong.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`, `cpt-cf-policy-engine-actor-tenant-policy-admin`

#### Assignment Authorization Across Barriers

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-assignment-authorization`

The system **MUST** authorise the creation, amendment, and withdrawal of an assignment against the caller's own security context, and **MUST** declare the barrier handling its assignment surfaces apply rather than inheriting a platform-wide default. The platform's position is that barrier enforcement is decided per resource type and per endpoint, so a gear that declares nothing has no rule at all.

The declared rule is that assignment surfaces **respect barriers**. A caller **MUST NOT** create, amend, or withdraw an assignment at a tenant that lies behind a self-managed barrier from its own context, and **MUST NOT** enumerate the assignments held at such a tenant. Both **MUST** be refused with the boundary cause of `cpt-cf-policy-engine-fr-cross-tenant` rather than reported as an absence: the platform already makes a descendant tenant's existence visible to its ancestor, so concealing the tenant here would conceal nothing and only make the refusal unexplainable.

This governs the management plane alone. What an assignment made at or above a barrier *governs* is decided by `cpt-cf-policy-engine-fr-inheritance-barriers` on the evaluation plane, from the caller's requested barrier handling and the assignment's own reach declaration, so a self-managed tenant is not exempt from an ancestor's guardrail. What the barrier withholds is the ancestor's ability to administer policy *inside* the subtree, which is a different question from whether policy it already holds reaches in.

- **Rationale**: Assignment is the one management operation whose target is a tenant rather than a piece of content, so `cpt-cf-policy-engine-fr-content-isolation` does not reach it and nothing else here did either. An undefined rule means the first implementation picks one by accident, and both accidents are bad: an ancestor silently able to attach policy inside a subtree that raised a barrier defeats the barrier as an autonomy boundary, while an ancestor unable to see the assignment it holds there cannot audit its own guardrails. Separating the two planes is what makes both halves defensible — the ancestor keeps the guardrail it is accountable for, and the self-managed tenant keeps administration of what happens inside it. Section 13's question about inheritance is a question about the evaluation plane; this requirement neither answers it nor depends on how it is answered.
- **Actors**: `cpt-cf-policy-engine-actor-tenant-policy-admin`, `cpt-cf-policy-engine-actor-platform-operator`

### 5.4 Policy Matching

#### Applicable Set Determination

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-applicable-set`

For each evaluation the system **MUST** determine the applicable set of policy documents by matching trigger type, resource type, evaluation phase, and attribute filters, restricted to active assignments visible from the requesting tenant context.

- **Rationale**: The applicable set is what makes evaluation cost independent of total policy volume. It is also the explanation an author needs when a policy does not fire.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`

#### Deterministic Ordering

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-deterministic-ordering`

The system **MUST** evaluate the applicable set in a total order. Proximity to the resource orders first, policy priority second, and a stable property of the assignment itself resolves the remainder, so that two assignments on the same tenant sharing a policy priority still evaluate in a fixed order.

- **Rationale**: The result does not depend on order, but three things a reader of a decision depends on do: which document a refusal names as responsible, how many documents were evaluated before short-circuiting stopped the loop, and therefore whether two runs of the same input produce identical records. Proximity and priority alone do not produce a total order, because nothing prevents two assignments on one tenant from carrying the same priority; without a third key those three become dependent on retrieval order, and an auditor comparing two records of the same decision sees a difference that means nothing.
- **Actors**: `cpt-cf-policy-engine-actor-security-auditor`

#### Evaluation Phases

- [ ] `p3` - **ID**: `cpt-cf-policy-engine-fr-evaluation-phases`

The system **MUST** evaluate documents in the after-the-operation phase without allowing their outcome to affect whether the operation proceeded, and **MUST** record those outcomes as it does any other.

- **Rationale**: Governance frequently needs to observe before it enforces. Without a phase whose outcome cannot refuse, every new rule is a production risk. The phase vocabulary itself is defined by `cpt-cf-policy-engine-fr-target-binding`; this requirement covers only the non-blocking behaviour, which is why it is not needed for a first release that ships the blocking phase alone.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`, `cpt-cf-policy-engine-actor-policy-author`

### 5.5 Decision Semantics

#### Permit Provenance

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-permit-provenance`

The system **MUST NOT** produce a permission from a failure. No error condition, unevaluable document, unreachable dependency, or unresolvable tenant context may yield a permit of any cause; every one of them refuses, per `cpt-cf-policy-engine-fr-denial-versus-failure`. There is one exception, and it is the same one `cpt-cf-policy-engine-fr-denial-precedence` carries: `cpt-cf-policy-engine-fr-emergency-access` permits on precisely these paths, under an entitlement resolved without consulting policy content, and stamps its own permission cause so that a permission obtained that way is never reported as governed or as ungoverned.

An operation that policy does not govern **MUST** be permitted, and that permission **MUST** be marked ungoverned. The system **MUST** make the ungoverned count observable, so that an operator can see how much of the estate policy is silent about.

- **Rationale**: Refusing what no policy governs is the wrong default for this gear: a deployment that has authored nothing would refuse every gated operation, and a resource type nobody wrote policy for would become unusable rather than unregulated. Permitting it is the honest answer — the gear was asked whether policy objects, and it does not. What must not happen is the two failure modes collapsing into that answer: a permit obtained because a dependency was down is not the same as one obtained because policy is silent, and only the first is a defect. Marking the ungoverned case is what keeps the silence visible instead of indistinguishable from approval, and counting it is what turns "we have no policy here" from an assumption into a number.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`, `cpt-cf-policy-engine-actor-platform-operator`

#### Denial Precedence

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-denial-precedence`

Where the applicable set yields both permitting and prohibiting outcomes, the system **MUST** refuse.

- **Rationale**: A prohibition is a stated intent to prevent something. Allowing a permission elsewhere to override it would make prohibitions unreliable and therefore unusable as a control.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`

#### Outcome Combination

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-outcome-combination`

A policy **document** yields exactly one outcome per evaluation, drawn from a closed, versioned set. At this priority the set is: permit, prohibit, and observe, which records without influencing the result. `cpt-cf-policy-engine-fr-deferral-outcome` adds a fourth member, and defines its own position in the rules below rather than leaving it to be inferred.

An **evaluation** yields exactly one result: permit or prohibit. The system **MUST** combine document outcomes into that result by these rules:

- any prohibit yields a prohibition;
- otherwise the result is a permission.

Every permission **MUST** carry a cause, drawn from a closed, versioned set, distinguishing the ways it arises. At this priority the set has two members: **governed**, where at least one document permitted, and **ungoverned**, where no document permitted or prohibited. An observe outcome never contributes, so an empty applicable set and one containing only observe outcomes are both ungoverned. `cpt-cf-policy-engine-fr-emergency-access` adds a third member, **emergency**, at its own priority tier, and defines when it is stamped rather than leaving it to be inferred; it is the one cause not derived from the outcome sequence at all, and while that requirement is unimplemented no path constructs it.

These rules are order-independent by construction: the result is the same whatever sequence the applicable set is evaluated in. Ordering is required for other reasons, stated in `cpt-cf-policy-engine-fr-deterministic-ordering`, and **MUST NOT** be relied on to change a result.

- **Rationale**: Consumers need one answer, not a list, and a two-valued answer is the one a caller can act on without a branch it might forget. The two facts a caller does need to keep apart — policy considered this and allowed it, versus no policy considered it — travel as a cause rather than as a third variant, on the same pattern the refusal side already uses. Stating order-independence in the requirement closes the gap that let two adjacent sections imply different algebras: prohibition wins over permission by precedence, not by position.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`

#### Short-Circuit Evaluation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-short-circuit`

The system **MUST** stop evaluating the applicable set once no remaining outcome can change the result, and **MUST** record how many documents matched alongside how many were evaluated.

- **Rationale**: Short-circuiting bounds evaluation cost, and a prohibition can be reached without examining the rest of the set. Recording both counts keeps that visible — otherwise an auditor cannot tell whether a policy was consulted and permitted, or never reached.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`, `cpt-cf-policy-engine-actor-security-auditor`

#### Denial Reason

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-denial-reason`

Every refusal **MUST** carry a machine-readable reason code drawn from the platform's error type system, and **MUST** permit an accompanying human-readable detail that identifies the policy responsible. The detail **MUST** be recorded, and **MUST NOT** be returned to end users.

- **Rationale**: Callers must branch on the reason without parsing prose, and Infrastructure Resource Manager goes further: its cascade admission requires a refusal to identify which condition fired, so that the caller can report it against the offending resource. A caller cannot do that unless the decision carries the identity of what refused. Detail describes the tenant's policy posture, so returning it to an end user discloses how the control is configured.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`

#### Denial and Failure Are Distinct

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-denial-versus-failure`

The system **MUST** report an infrastructure failure distinctly from a policy refusal, and **MUST NOT** represent an evaluation failure, an unavailable evaluation backend, a hierarchy provider outage, or an internal error as a prohibiting decision. Nor **MUST** it represent any of them as a permission of any cause: an error means the gear could not evaluate, which is not the same as evaluating and finding nothing, and a caller that read the two alike would proceed with an operation because a dependency was down.

- **Rationale**: Both outcomes block the operation, but they demand different responses: a refusal is correct behaviour, an outage is an incident. Conflating them hides outages from alerting and tells users their policy changed when it did not. Infrastructure Resource Manager depends on this distinction directly: it requires an unreachable decision service to be retried as transient rather than treated as a refusal.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`, `cpt-cf-policy-engine-actor-platform-operator`

#### Deferral Outcome

- [ ] `p3` - **ID**: `cpt-cf-policy-engine-fr-deferral-outcome`

The system **MUST** support a document outcome that neither permits nor prohibits but defers the operation for human review, and **MUST** return the corresponding result distinguishably from both a permission and a prohibition. Its position in `cpt-cf-policy-engine-fr-outcome-combination` is between the two: any prohibit still yields a prohibition; otherwise any defer yields a deferral; otherwise the result is a permission with its cause as before. A deferral therefore overrides a permit and is overridden by a prohibit. Shipping it makes the result three-valued for the first time, but not for the first time *expressible*: `cpt-cf-policy-engine-interface-decision-client` reserves the variant unpopulated from its first version, so shipping widens a reserved shape rather than adding one, and the gateway reserves it likewise under `cpt-cf-admission-control-fr-deferral-relay`. What the reservation does not make cheap is the behavioural change — a caller that received a refusal will begin receiving a deferral — so serving it remains a versioned change on both surfaces.

- **Rationale**: A deferral is not a refusal — the operation proceeds once someone approves it. Conflating them either blocks legitimate work or bypasses the review entirely, and the conflation is not hypothetical: the gateway's rule for an engine result it cannot map is to refuse with an infrastructure cause, so a deferral reaching a gateway with nowhere to put it would be reported as an outage. Reserving the variant on both contracts before either serves it is what stops that, and it costs nothing while no engine emits one. This is p3 because nothing in the tree currently routes a deferral to an approver.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`

#### Emergency Access

- [ ] `p3` - **ID**: `cpt-cf-policy-engine-fr-emergency-access`

The system **MUST** permit an operation that policy would otherwise prohibit when all of the following hold: the request explicitly asserts emergency access, the subject holds the emergency entitlement, and the entitlement is resolvable without evaluating policy content.

This is the sole exception to three requirements, and all three **MUST** be read as admitting it: `cpt-cf-policy-engine-fr-denial-precedence`, which a permission over a prohibiting outcome contradicts; the fail-closed threshold of `cpt-cf-policy-engine-nfr-fail-closed`, which already excludes and separately measures this path; and `cpt-cf-policy-engine-fr-permit-provenance`, whose prohibition on producing a permission from a failure this requirement overrides on exactly the paths that failed. The exception is available only on the paths those three would otherwise refuse — it **MUST NOT** apply where the refusal came from an unresolvable tenant context or a cross-tenant boundary, which no emergency entitlement may cross.

A permission reached this way **MUST** carry the **emergency** cause that `cpt-cf-policy-engine-fr-outcome-combination` reserves for it, and **MUST NOT** carry governed or ungoverned. Neither fits and neither is a harmless approximation: documents prohibited, so the permission is not ungoverned, and none permitted, so it is not governed. That cause is the record marker required by `cpt-cf-policy-engine-fr-decision-records` — one field rather than a cause plus a flag beside it — and the decision **MUST** additionally increment a distinct metric.

- **Rationale**: Incidents occur where correct policy blocks necessary recovery. The entitlement must resolve without evaluating content, because the incident being recovered from may be the content itself — an override that depends on the thing that is broken is not an override. An unmarked one is indistinguishable from an attack, so both the cause and the metric are part of the requirement rather than consequences of it. Naming all three exceptions matters more here than the tidiness of it: this is the only requirement in the document that permits where another requirement forbids, so a reader who finds only two of the three named has to guess whether the third is an oversight or a contradiction. Carrying the mark as a cause rather than as a separate flag is what stops the two disagreeing — a permission whose cause said governed while a flag said emergency would be a decision no auditor could classify, and the emergency permission is the single decision in this gear most likely to be disputed.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`, `cpt-cf-policy-engine-actor-security-auditor`

### 5.6 Evaluation Inputs and Outputs

#### Authorization Boundary

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-authorization-boundary`

The system **MUST NOT** be usable as an authorization decision path, and **MUST** make that structural rather than declared. Two obligations follow.

The evaluation input **MUST** carry the subject as an identifier together with the subject's tenant, and **MUST NOT** carry the subject's entitlements — no role, no role binding, no permission or scope, no group membership, and no resolved statement of what the subject may do. The system **MUST NOT** resolve an entitlement in the course of an evaluation, and where it resolves one outside evaluation it **MUST NOT** pass it in — which is the discipline `cpt-cf-policy-engine-fr-emergency-access` already follows, since the gear consults that entitlement itself and policy content never sees it.

A decision **MUST NOT** widen access. The result of an evaluation states that policy does or does not object to an operation; it is never a statement that the subject is authorized to perform it, and no cause on a permission — governed, ungoverned, or the emergency cause of `cpt-cf-policy-engine-fr-emergency-access` — may be represented as one.

**Conformance expectation on the caller**, not observable by this gear: authorization is decided before admission, and a permission from this gear is no substitute for it. A caller that gates an operation on this gear alone has performed no authorization check at all.

- **Rationale**: The input of `cpt-cf-policy-engine-fr-evaluation-input` — subject, action, resource, and arbitrary caller-supplied context — is the shape every policy decision point takes, so the input alone cannot distinguish an admission question from an authorization one. Section 4.2 places authorization outside this gear in prose, and prose is not a boundary: nothing in the content model would stop an author writing rules over roles, which would put a second answer to "who may do what" in a component with its own lifecycle, its own audit trail, and no obligation to agree with `authz-resolver`. Withholding entitlements from the input is what makes such a rule unwritable rather than merely discouraged — content that cannot see a role cannot decide by role. The distinguishing test the two kinds actually admit is therefore about inputs and not about intent: a rule decidable from the subject's grants alone is authorization, and a rule needing the proposed values of the change is admission, which is the one this gear is given the facts for. Two holes remain, and are stated rather than implied: an author may still write a rule naming a subject identifier, and a caller may put anything into the operation context, which the gear has no schema for and cannot classify. Neither is closed here, and neither escalates — because a decision cannot widen access, the worst either produces is a refusal narrower than authorization already permitted, which is the only direction an admission control is allowed to travel. Adding an authorization content kind later would be the versioned change to the closed set that `cpt-cf-policy-engine-fr-document-kinds` describes, governed by the positions in [arch/authorization/](../../../../docs/arch/authorization/), and not a configuration option.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-security-auditor`, `cpt-cf-policy-engine-actor-enforcing-gear`

#### Evaluation Input Contract

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-evaluation-input`

The system **MUST** accept, on each evaluation request, the subject and the subject's tenant, the action, the resource type and the resource identifier where one exists, the tenant context including barrier handling and status filtering, a caller-supplied operation context carrying the resource properties policy is to judge, and the caller's correlation identifier. The subject arrives as identity alone — an identifier and a tenant — because `cpt-cf-policy-engine-fr-authorization-boundary` forbids the input from carrying that subject's entitlements, and it arrives on the input without being trusted from it: `cpt-cf-policy-engine-fr-subject-authenticity` derives the subject and its tenant from the caller's propagated security context and refuses a request whose asserted values disagree. The system **MUST** additionally stamp each evaluation with the timestamp it will supply to the backend, and treat that timestamp as an input to the decision rather than as metadata about it. The system **MUST NOT** retrieve resource state from the consumer in order to evaluate. It **MUST NOT** generate a correlation identifier of its own under any circumstances, and **MUST** refuse an evaluation that arrives without one, reporting a caller defect distinctly from both a policy refusal and an infrastructure failure. Minting a substitute is not the lenient option it appears to be: the value's whole purpose is to match this gear's decision record to the gateway's admission record of the same gated operation, on the audit topic they share. A locally minted value matches no such record, so it produces a row that satisfies the field set of `cpt-cf-policy-engine-fr-decision-records` and still cannot be tied to the operation it describes — worse than an absent value, which at least reads as incomplete.

- **Rationale**: Policy that judges a resource the consumer is about to create cannot read that resource — it does not exist yet, which is the whole point of judging before the operation. Making the input complete is also what keeps the gear free of the resource-synchronisation coupling that the platform rejected for authorization, and what makes an evaluation reproducible from its record. The correlation identifier is supplied rather than minted because this gear sees one evaluation and the gateway sees the whole gated operation. One gated operation leaves two records — the gateway's, saying what was gated and how it was answered, and this gear's, saying how policy reached that answer — and the correlation identifier is the only field they share, so it is the only thing that shows the two describe one event. An identifier minted here would be unique and present and would match no admission record at all. Accepting it is safe because of who the caller is and what that caller is required to do with it. The only caller is the gateway, per `cpt-cf-policy-engine-fr-subject-authenticity` and the single-path constraint of Section 4.2, and `cpt-cf-admission-control-fr-admission-records` requires the gateway to mint the value per gated operation and forbids it from deriving the value from anything the calling gear or its client controls. The identifier therefore arrives on the request while still being outside the reach of the subject being audited — which is the test an audit key has to pass, and which a value chosen by the party under audit would fail. It is deliberately not a tracing identifier for that same reason: a trace identifier is one the originating client may set, repeat, or omit, so it satisfies neither the presence requirement nor the reach requirement, however convenient the join would be.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`

#### Subject Authenticity

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-subject-authenticity`

The system **MUST** require the caller's propagated security context on every evaluation and on every batch, and **MUST** refuse an evaluation that arrives without one. The subject and the subject's tenant **MUST** be taken from that context rather than believed from the request: where the evaluation input also carries them, the system **MUST** compare them against the context and **MUST** refuse a mismatch with a cause distinct from both a policy refusal and an infrastructure failure. A refused mismatch **MUST** be recorded under the context's own subject, and the system **MUST NOT** evaluate content, return a decision, or write a record that attributes an evaluation to an identity the caller did not present.

The system offers no on-behalf-of mode, and this follows the platform's own model rather than deferring a capability. The platform's two-plane authentication decision states that tenant-scoped work initiated by a system actor with no human behind it carries an ordinary tenant security context — obtained from a user token or from a service-to-service client-credentials token — and is authorized as itself. A caller performing work queued, scheduled, or retried earlier therefore presents either the requesting subject's context, where it still holds one, or its own service context; in the second case the decision and its record name that service, and the join back to the subject who asked is the correlation identifier of `cpt-cf-policy-engine-fr-evaluation-input`, which the earlier record carries too. What the platform has no representation for is the third thing — one subject acting for another — because the security context carries no actor field, so an on-behalf-of mode would need both that field and an authority able to sanction it.

Where a deployment runs with authentication disabled, the context is the platform's anonymous context and its default subject and tenant are what the system derives; a caller **MUST NOT** assert a different subject there either. The comparison is therefore unconditional rather than configurable: what changes when authentication is disabled is which subject the context carries, never whether the request's own subject is believed instead.

- **Rationale**: `cpt-cf-policy-engine-fr-evaluation-input` accepts the subject as part of the request, and a field the caller controls is a field the caller can choose. Everything downstream rests on the subject being real: the decision record is the audit trail, the violations projection is filtered by tenant, and `cpt-cf-policy-engine-fr-cross-tenant` decides reachability from the requesting context — so an input trusted as supplied would let a caller obtain a decision, and leave evidence, for an identity that never asked, and would let it choose a subject tenant that widens the subtree the evaluation may reach. Deriving both from the propagated context turns those fields into a restatement of something already authenticated; comparing rather than silently overwriting keeps a caller's mistake visible instead of absorbed, which matters because a mismatch is either a defect in the caller or an attempt, and both are worth seeing. None of this makes the gear an authorization decision point: `cpt-cf-policy-engine-fr-authorization-boundary` still withholds every entitlement of the subject, and what the context supplies here is identity, which that requirement already both permits and requires.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`, `cpt-cf-policy-engine-actor-security-auditor`

#### Batch Evaluation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-batch-evaluation`

The system **MUST** accept several evaluation requests as one batch and **MUST** return one combined verdict alongside the individual outcomes, such that a prohibition on any member yields a prohibition for the batch. The combined verdict **MUST** identify every member that was refused, not only the first.

- **Rationale**: A consumer admitting a plan needs one answer for the whole plan, and needs it to be the same answer preview gave. Issuing one request per member works against both: it multiplies the fixed cost of an evaluation — tenant resolution, applicable-set determination — across every member, and it leaves the caller to combine partial answers itself, which is where two callers start combining them differently. Returning every refused member rather than the first is what lets a caller report the whole problem at once instead of one round trip per fault.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`

#### Obligations

- [ ] `p3` - **ID**: `cpt-cf-policy-engine-fr-obligations`

The system **MUST** allow a permitting decision to carry obligations the caller is required to honour, and each obligation **MUST** carry a stable identifier from the platform's type system. The obligation set is open rather than closed, so obligation identifiers are GTS identifiers resolved through the types registry rather than free-form strings. How a caller must behave on an identifier it does not recognise is a conformance expectation of the decision contract rather than gear behaviour, and is stated in `cpt-cf-policy-engine-interface-decision-client`; this gear cannot observe it.

- **Rationale**: Some policy permits an action only under conditions the decision point cannot enforce itself. An obligation the caller does not understand must be rejected rather than ignored, so obligations must be identifiable rather than free-form, and a typed identifier is identifiable in a way a string is not.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`, `cpt-cf-policy-engine-actor-types-registry`

### 5.7 Expression Evaluation

#### Expression Evaluation Through the Shared Library

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-expression-evaluation`

The system **MUST** evaluate policy content through the platform's shared evaluation facility, selecting the backend the content declares, and **MUST NOT** implement a policy language or an evaluator of its own.

- **Rationale**: A gear that implements its own evaluator acquires a language surface to secure, bound, and version, and the platform already has two gears that each built one. Linking the shared facility instead keeps the backend contract in one place: the gear gains nothing by rebuilding a registry the facility already provides, and a deployment that needs a different language changes a backend rather than a gear. This is why the direct dependency is not a loss of optionality — the facility is where optionality lives, and it is the level at which a second language becomes available to every consumer at once rather than to one gear.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`

#### Evaluation Isolation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-evaluation-isolation`

The system **MUST NOT** pass any capability into evaluation: the backend receives the data supplied on the evaluation request, the policy content, and an evaluation timestamp the gear supplies, and nothing else — no client, no connection, no handle, and no clock. The system **MUST NOT** select a backend whose registration in `cpt-cf-policy-engine-contract-gts` does not declare a sandbox property covering both capability isolation and determinism. That declaration is made by the evaluation facility about a specific **build** of a backend, not by the backend library about itself: a policy-language implementation states no such guarantee, and its exposure is decided by which compile-time features the facility enables. Whether a backend honours the declaration is a property of the facility, which Section 4.2 places outside this gear; what this requirement governs is what the gear hands over, which backends it is willing to use, and what content it refuses to activate.

Because determinism cannot be obtained by feature selection alone, the system **MUST** additionally reject content that references a non-deterministic builtin — a clock reader, a random number or identifier generator — and **MUST** reject it at validation and activation rather than at evaluation. A backend may register such builtins behind the same feature that makes it usable at all, in which case they cannot be excluded from the build; and a backend that offers no way to remove a builtin, and that resolves builtins ahead of any extension registered over the same name, cannot be constrained from the outside at call time either. A denylist checked where `cpt-cf-policy-engine-fr-content-validation` already resolves identifiers is then the only enforcement point that exists. The denylist is a property of the declared backend build and **MUST** be revised with it.

- **Rationale**: Policy content is authored by tenant administrators and evaluated inside the platform's admission path. Content that could reach the network or the filesystem would make policy authoring a remote execution surface, and content whose result varied between two evaluations of the same input would make decision records unreproducible and short-circuiting unsafe. The two halves need different mechanisms, which is why they are stated separately. Capability isolation is obtained by handing over nothing, and it holds by construction. Determinism is not: it depends on which builtins the backend build registers, and an audit of the first candidate found a clock reader and two random generators present, one of them inseparable from the feature that makes the backend function. Requiring the facility to declare the property rather than the backend is the honest form of the rule — a declaration no implementation makes is a rule that silently never applies — and requiring the gear to refuse the content is what makes the guarantee real rather than declared.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`, `cpt-cf-policy-engine-actor-security-auditor`

#### Bounded Evaluation Cost

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-evaluation-cost-bounds`

The system **MUST** bound the wall-clock time spent evaluating a single policy document and the wall-clock time spent on a single evaluation as a whole, **MUST** make both bounds operator-configurable, **MUST** expose each bound and each exceedance as a metric, and **MUST** refuse the evaluation when a bound is exceeded rather than continuing.

Defaults are 2 milliseconds per document and 4 milliseconds per evaluation. Both are derived from `cpt-cf-policy-engine-nfr-decision-latency` rather than chosen independently, and the derivation is the constraint: p95 must be met **by** the hierarchy-miss path, a miss consumes up to the 20 milliseconds `cpt-cf-policy-engine-fr-dependency-timeouts` allows, and roughly 5 milliseconds therefore remain for the whole of the rest of an evaluation. The per-evaluation bound takes 4 of those; matching, outcome combination, and record construction — all in memory — take the remainder. The per-document bound is set below the per-evaluation one because a bound equal to it would leave no room for a second document.

- **Rationale**: An expression is authored content, so its cost is not under the gear's control. Without a bound, one tenant's expression makes every tenant's admission slow, and the latency requirement becomes unenforceable. `quota-enforcement` reached the same conclusion for its own engines and fixed an operator-tunable per-policy timeout. The figures have to close against the latency budget rather than being set to a comfortable round number: a per-evaluation default larger than the miss-path allowance would let an evaluation stay inside every documented default and still breach the p95 target, which makes the target unenforceable by exactly the configuration the gear ships with — and unenforceable in the direction hardest to notice, since every individual setting would look compliant.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`

#### Dependency Timeouts

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-dependency-timeouts`

The system **MUST** bound the time it will wait on each hierarchy provider and **MUST** refuse when a bound is exceeded rather than waiting indefinitely. The bound is operator-configurable with a default of 20 milliseconds, which is the figure `cpt-cf-policy-engine-nfr-decision-latency` allocates to a hierarchy miss.

- **Rationale**: The fail-closed requirement names hierarchy timeouts as a condition that must refuse, which presupposes a timeout exists to be exceeded. A hung dependency otherwise stalls the request rather than refusing it, turning a bounded outage into an unbounded one.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`, `cpt-cf-policy-engine-actor-hierarchy-provider`

#### Responsibility Boundary

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-responsibility-boundary`

The system **MUST** own policy content, lifecycle, assignment resolution, hierarchy access, outcome combination, and decision recording, and **MUST** delegate to the evaluation facility only the interpretation of a document's content. The system **MUST** validate every outcome a backend returns against the closed outcome set before combining it, and **MUST** reject a result it cannot map to that set.

- **Rationale**: Keeping semantics in the gear and language in the facility is what allows the facility to serve consumers with nothing to do with policy. Validating returned outcomes at the boundary matters more with a policy language than it would with a bare expression evaluator, because a policy document can return an arbitrary shape rather than a boolean: an evaluation result trusted without checking makes a backend defect into a policy bypass. This is the strict-boundary position `quota-enforcement` took for its engines, applied one layer out.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`

### 5.8 Multi-Tenancy

#### Policy Content Isolation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-content-isolation`

The system **MUST** confine policy content reads and writes to the requesting tenant and the tenants it is entitled to manage, and **MUST** make content belonging to other tenants indistinguishable from content that does not exist.

- **Rationale**: Policy content describes a tenant's security posture. A response that distinguishes forbidden from absent confirms existence and leaks structure.
- **Actors**: `cpt-cf-policy-engine-actor-tenant-policy-admin`

#### Cross-Tenant Evaluation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-cross-tenant`

The system **MUST** refuse an evaluation whose resource tenant lies outside the subtree the subject's context is entitled to reach under the requested tenant mode, barrier handling, and status filtering, and **MUST** identify that refusal cause distinctly from an ordinary absence of permitting policy.

- **Rationale**: A resource in a descendant tenant is the normal case in a hierarchy, so "different tenant" is not the test. The test is whether the resource lies within the reachable subtree once barriers and tenant status are applied. Access outside that boundary is the highest-consequence failure in a multi-tenant platform, so it needs its own refusal cause rather than being reported as no policy matched.
- **Actors**: `cpt-cf-policy-engine-actor-enforcing-gear`, `cpt-cf-policy-engine-actor-security-auditor`

#### Decision Record Visibility

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-record-visibility`

The system **MUST** scope decision records and the violations projection by the resource tenant the record carries under `cpt-cf-policy-engine-fr-decision-records`, and **MUST NOT** scope them by the tenant that owns the content which produced them. The two diverge whenever an ancestor's bundle refuses an operation in a descendant, which inheritance makes the ordinary case rather than the exception.

A caller **MUST** be able to read a record whose resource tenant lies within the subtree reachable from its own context, and **MUST NOT** be able to read one outside it. As with assignment, the system **MUST** declare the barrier handling these surfaces apply rather than inheriting a default: record surfaces **respect barriers**, so an ancestor **MUST NOT** read the records of a tenant behind a self-managed barrier even where the responsible document belongs to the ancestor's own bundle. The system **MUST** offer reading across a barrier only under a distinct platform-level capability, **MUST NOT** imply that capability from any tenant-level one, and **MUST** record every read performed under it as an administration event.

Where a refusal in a tenant was produced by an ancestor's document, the system **MUST** return that document's identity to a reader entitled to the tenant's records, and **MUST NOT** thereby expose the document's content, the other documents of its bundle, or the assignment behind it. `cpt-cf-policy-engine-fr-content-isolation` continues to govern every content read: an identity is not content.

- **Rationale**: A record describes an operation on a resource, so it belongs to the tenant of that resource and not to whoever wrote the rule. Scoping by content ownership would hand an ancestor the whole subtree's decision history and hand the tenant whose resources were judged nothing, which is backwards on both sides — and it is the divergence `cpt-cf-policy-engine-nfr-tenant-isolation` already assumes when it says content scopes on the owner and records scope on the resource tenant. Barriers have to be declared rather than assumed because the platform makes audit visibility across one a per-endpoint choice: a record carries a descendant's subject, action, and resource identity, which is exactly the business data a barrier exists to withhold, so the default respects it and the exception is separate, narrow, and attributable. Naming the responsible document to the tenant that was refused is the other half of the rule — a tenant told only that policy refused something cannot act on it, and the document identity is what makes a refusal explainable without disclosing an ancestor's rules.
- **Actors**: `cpt-cf-policy-engine-actor-tenant-policy-admin`, `cpt-cf-policy-engine-actor-security-auditor`, `cpt-cf-policy-engine-actor-platform-operator`

#### Administration Authorization

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-admin-authorization`

The system **MUST** authorise every management operation against the caller's own security context, distinguishing at minimum the ability to read policy content, to author and modify drafts, to activate and withdraw bundles, to assign bundles to a tenant, to read decision records, and to read the violations projection. A caller **MUST NOT** be able to grant itself, or any subject, an entitlement it does not itself hold.

The two read capabilities are separate from content read and from each other, and neither is implied by it. They are also scoped differently: `cpt-cf-policy-engine-fr-record-visibility` scopes both by the resource tenant a record carries rather than by the tenant that owns the content, and the cross-barrier read it defines is a further capability again, held at platform level and never with a tenant-level one.

- **Rationale**: This gear decides what the platform refuses, which makes its own administration surface a high-value target. Undefined self-authorization means the first implementation invents it, and a privilege-escalation path through policy authoring defeats every policy the gear enforces. Separating content read from authoring, from activation, from assignment, and from the two record reads matters because they have different blast radii and are held by different people: a compliance reader needs decisions and refusals and no content at all, while a policy author needs content and has no business reading the estate's decision history.
- **Actors**: `cpt-cf-policy-engine-actor-tenant-policy-admin`, `cpt-cf-policy-engine-actor-platform-operator`, `cpt-cf-policy-engine-actor-policy-author`

#### Bootstrap

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-bootstrap`

The system **MUST** provide a path by which the first policy content can be created in a deployment that has none, without that path depending on a decision the gear cannot yet make.

- **Rationale**: A deployment starts with no content and no assignment, and the first bundle has to be creatable by someone. The management surface is authorised through the platform authorization path rather than by this gear's own content, so the cold start is not circular — but it is undefined, and an undefined cold start is where implementations invent an undocumented backdoor that then survives into production. Naming the path is what stops that.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`

### 5.9 Observability

#### Decision Records

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-decision-records`

The system **MUST** emit a record for every evaluation. Within this gear, this requirement owns the normative field set and no other requirement here restates it; the gateway's admission record is governed by that gear and is neither restated nor constrained here. Every record **MUST** carry:

- the correlation identifier the caller supplied under `cpt-cf-policy-engine-fr-evaluation-input`, which is what matches this record to the gateway's admission record of the same gated operation, and the batch identifier where the evaluation was part of a batch, which matches the batch as a group
- an evaluation identifier, unique per evaluation, under which the record is written exactly once
- the subject and the subject's tenant, as the opaque identity-provider references of `cpt-cf-policy-engine-fr-subject-data-handling`
- the resource's tenant, which is the tenant the record is scoped and filtered by and is not always the subject's
- the action
- the resource type, and the resource identifier where the evaluation named one
- the decision, and for a permission its cause: governed, ungoverned, or — once `cpt-cf-policy-engine-fr-emergency-access` ships — emergency. That third value is also the marker that requirement calls for, so the emergency path adds no field of its own to this set
- the refusal cause, where the decision is negative, and the identity of the policy document that produced it
- the identity and version of the policy content that determined the outcome
- the identity and version of every bundle that took part in the evaluation, not only the one that determined it
- the identity and version of the evaluation backend that interpreted the content
- the count of documents that matched and the count actually evaluated, which differ when evaluation short-circuits
- the names of the operation-context properties the evaluation read, in the form `cpt-cf-policy-engine-fr-record-confidentiality` permits
- the evaluation timestamp supplied to the backend, without which a decision that turned on time cannot be reproduced
- the elapsed time

- **Rationale**: This is the evidence base for compliance and incident review, and it is also the substrate the refusals projection reads. Recording only refusals loses the permit that mattered; omitting the matched-versus-evaluated counts hides that short-circuiting left policy unexamined. Two fields exist because a decision is not a function of one input: recording every participating bundle is what keeps attribution from silently degrading where several assignments apply along a tenant chain, which is exactly the configuration a single determining-version field describes worst; and recording the backend version is what makes a decision reproducible at all, since the same content under a different interpreter is a different decision. Field sets that appear in more than one place drift, so every field any other requirement depends on is enumerated here and referenced from there — including the tenant the projection filters by, the emergency permission cause, and the context projection, each of which another requirement needs and none of which may be added independently of this list.
- **Actors**: `cpt-cf-policy-engine-actor-security-auditor`, `cpt-cf-policy-engine-actor-audit-sink`

#### Violations

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-violations`

The system **MUST** expose refusals as a queryable projection over the prohibiting decision records within the retention window, filterable by the resource tenant, by policy document, by resource type, and by time range, and **MUST** return the retention window in force alongside every result. Which of those records a given caller may see is governed by `cpt-cf-policy-engine-fr-record-visibility`, and the capability required to ask at all by `cpt-cf-policy-engine-fr-admin-authorization`. A projected refusal **MUST NOT** be separately stored state, and the system **MUST NOT** evaluate policy against existing resources in order to produce one. The projection reports what policy has refused, not what currently violates policy: an entry does not clear when the underlying condition is corrected, and it disappears when it ages out of retention. The surface is named for what it returns — refusals within a window, not conditions currently in breach — and **MUST** distinguish an empty result from one truncated by that window.

The projection covers refusals this gear produced, and no others. Refusals reached without an evaluation — a gateway's own built-in policy, or a gateway refusing because it could not reach this gear — leave no decision record here and **MUST NOT** appear. The system **MUST** state that boundary on the surface rather than leaving an absence to be read as an all-clear.

- **Rationale**: An administrator whose policy refused something needs to find it without reading logs, and this is the third surface the gear's management API is required to offer. Deriving it from records rather than storing it keeps one source of truth and avoids a second lifecycle to keep consistent. Excluding evaluation against existing resources is the boundary that stops the projection from becoming a compliance scanner over an estate this gear does not own — but that boundary has a cost, and stating it in the requirement is what stops a reader from mistaking a refusal history for a standing compliance view. The second boundary is stated for the same reason: this gear cannot report a refusal it never made, and the two classes it misses are the ones an administrator is least likely to guess are missing — a gateway's built-in refusal is invisible here because the evaluation never happened, and a refusal caused by this gear being unreachable is invisible here precisely because this gear was unreachable. An administrator reading an empty projection during an outage would otherwise conclude that nothing was refused, when in fact everything was. Whether a standing view is wanted is a separate capability with its own requirement, and Section 12 carries the risk that this projection is not what administrators actually ask for.
- **Actors**: `cpt-cf-policy-engine-actor-tenant-policy-admin`, `cpt-cf-policy-engine-actor-security-auditor`

#### Record Confidentiality

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-record-confidentiality`

Decision records **MUST NOT** contain bearer tokens or other subject credentials, and **MUST NOT** contain the values of caller-supplied operation context. Where a record refers to that context it **MUST** carry only the names of the properties the evaluation read.

- **Rationale**: Audit records are widely readable by design. A record carrying a credential turns the audit trail into a credential store, and one carrying submitted property values turns it into a data leak — those values are supplied by the caller, the gear has no schema for them, and a gear that cannot classify a value cannot decide whether it is safe to keep. Recording the property names instead is what an auditor actually needs, since the question is which facts the policy judged, not what they were; and it makes the rule checkable by inspecting the record shape rather than by evaluating an entitlement the gear has no way to resolve.
- **Actors**: `cpt-cf-policy-engine-actor-audit-sink`

#### Subject Data Handling

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-fr-subject-data-handling`

The personal data this gear holds is confined to the decision record: the subject identifier, the subject's tenant, and the redacted operation-context projection. The system **MUST NOT** collect subject attributes beyond the field set of `cpt-cf-policy-engine-fr-decision-records`, and **MUST NOT** store any profile attribute of a subject — no name, no address, no credential, no contact detail.

The subject identifier the system records is the opaque identity-provider reference the platform's security context carries, and the system **MUST** treat it as such: it **MUST NOT** resolve it to a person, **MUST NOT** enrich it, and **MUST NOT** offer an erasure operation over it. Erasure of a subject is performed by the identity provider, which [the platform's accepted position on user identity](../../account-management/docs/ADR/0005-cpt-cf-account-management-adr-idp-user-identity-source-of-truth.md) places as the sole source of truth for user identity and the sole handler of right-to-erasure requests. A record whose subject has been erased there keeps its reference, which then resolves to nobody — the orphaned-reference outcome that position already accepts for every other gear holding one.

The only bound this gear places on how long such a reference persists is `cpt-cf-policy-engine-fr-record-retention`.

- **Rationale**: Policy content is about rules, not people, so the record is the gear's only personal-data surface and it is worth confining explicitly. What this requirement deliberately does **not** do is give the gear an erasure mechanism of its own, and the omission is load-bearing. The obvious alternative — rewriting a subject's identifiers in place on request — fails twice. It assigns to a gear an obligation the platform assigns to the identity provider, so two components would answer the same request by different mechanisms with no rule about which wins. And it would not work: the gear's own table can be rewritten, but the copy it exports to the shared audit topic cannot — `event-broker` events are append-only and never modified, and the topic retains records *beyond* this gear's own window — so a rewrite would clear the short-lived copy, leave the long-lived one, and report success. Recording an opaque reference and letting the identity provider erase behind it needs no rewrite anywhere, which is the only reason the position is compatible with an append-only audit stream.

  Bounding the reference's lifetime by retention is what keeps the position honest. An orphaned reference is not nothing: it still records that *somebody* did something, and it remains joinable to other records carrying the same reference. Retention is therefore the gear's only real contribution to data minimisation here, and it is stated rather than implied.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`, `cpt-cf-policy-engine-actor-security-auditor`

#### Decision Record Retention

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-record-retention`

The system **MUST** apply a configurable retention period to decision records, **MUST** remove records beyond it, and **MUST** keep the retention period and the current record volume observable.

- **Rationale**: Decision records accumulate at request rate and carry subject identifiers, so unbounded retention is both a storage problem and a data-protection one. The retention period also bounds the violations projection, which makes it a user-visible setting rather than an internal one. A retention period that is configurable but unobservable is indistinguishable from none.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`, `cpt-cf-policy-engine-actor-security-auditor`

#### Operational Metrics

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-metrics`

The system **MUST** expose metrics covering decision latency, decision outcomes by cause, hierarchy provider latency and failures, expression evaluation cost and bound exceedances, cache effectiveness, and every fail-closed refusal by cause.

- **Rationale**: The gear fails closed, so its failures present as user-visible refusals rather than errors. Without a fail-closed counter separated by cause, an outage is indistinguishable from a policy change.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`

#### Evaluation Explanation

- [ ] `p3` - **ID**: `cpt-cf-policy-engine-fr-explanation`

The system **MUST** support explaining a decision on request, reporting which documents were applicable, which were evaluated, and which determined the outcome, without changing the decision itself.

- **Rationale**: The most common question about a policy engine is why it did what it did. Answering it by reading policy content does not scale past trivial configurations.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`, `cpt-cf-policy-engine-actor-security-auditor`

#### Evaluation Without Enforcement

- [ ] `p3` - **ID**: `cpt-cf-policy-engine-fr-dry-run`

The system **MUST** support evaluating a draft bundle against a supplied request without activating the bundle and without the result affecting any live decision.

- **Rationale**: Activation is otherwise the only way to learn what a policy refuses, which makes every change a production experiment. Dry-run turns the blast radius of a new rule into something an author can measure beforehand.
- **Actors**: `cpt-cf-policy-engine-actor-policy-author`

### 5.10 Configuration

#### Operational Limits

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-fr-operational-limits`

The system **MUST** enforce configurable bounds on policy content size, on document count per bundle, on the applicable set per evaluation, and on batch size, and **MUST** fail an operation that exceeds a bound rather than degrading decision latency.

- **Rationale**: Without bounds, one tenant's policy growth becomes every tenant's latency problem, and the latency target becomes unenforceable. Batch size needs its own bound because a batch multiplies a single request's cost by its member count.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`

#### Configuration Validation

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-fr-configuration-validation`

The system **MUST** reject unknown configuration keys at startup and **MUST** apply documented defaults to every optional setting.

- **Rationale**: A misspelled key that is silently ignored leaves an operator believing a limit is in force when it is not.
- **Actors**: `cpt-cf-policy-engine-actor-platform-operator`

## 6. Non-Functional Requirements

> Project-wide baselines for performance, security, reliability, and scalability are defined at the repository level in [ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md) and the foundational [guidelines/](../../../../guidelines/). This gear has no parent gear PRD. Only gear-specific NFRs appear below.

Requirements below are grouped by the platform's five quality vectors — efficiency, reliability, performance, security, and versatility — so that a vector carrying no gear-specific requirement appears as a stated position rather than as an absence a reader has to notice.

### 6.1 Efficiency

Cost itself is not modelled per gear in this repository, which Section 6.6 records as an exclusion. What is gear-specific is the memory one instance holds, because that is the only resource this gear consumes which no other requirement bounds.

#### Instance Footprint

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-nfr-instance-footprint`

The system **MUST** bound the memory one instance holds, **MUST** make that figure observable, and **MUST NOT** allow it to become the constraint that limits deployment size before the decision latency target does.

- **Threshold**: Resident snapshot memory and transient per-evaluation memory are bounded by the content limits of `cpt-cf-policy-engine-fr-operational-limits` and by nothing else, so the requirement is stated against those limits rather than as an absolute ceiling. At the limits in force: snapshot memory grows no worse than linearly in the product of governed tenant count and content volume per tenant; one evaluation's transient allocation is bounded by the size of the content it evaluates; and both figures are exported. Both are measured against tenant count and content volume, not against request rate alone, in the exercise `cpt-cf-policy-engine-nfr-scalability` already requires. An absolute ceiling is deliberately absent until the limits themselves are ratified, which Section 13 tracks, because a figure chosen ahead of them would be re-derived rather than met.
- **Rationale**: Two other requirements already rest on this being bounded and neither bounds it. The in-process clause of `cpt-cf-policy-engine-nfr-availability` is verified partly as "bounded memory under the content limits", and Section 11 assumes evaluation memory is bounded by those limits and by nothing else — an assumption that holds only if someone measures it. `cpt-cf-policy-engine-nfr-scalability` does not: it governs load and concurrency, and an instance can satisfy every latency and throughput target while approaching host exhaustion, because snapshot memory scales with governed tenants and stored content rather than with request rate. That is the failure this requirement exists to make visible, and its first symptom without measurement is the host dying rather than a threshold being missed. Stating it against the limits instead of as a byte figure is what keeps it honest while those limits are still open.
- **Verification Method**: Footprint measurement across tenant count and content volume, run as part of the scalability exercise rather than as a separate benchmark.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

### 6.2 Reliability

#### Fail-Closed Determinism

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-nfr-fail-closed`

The system **MUST** refuse on every error path, and **MUST NOT** expose any configuration that converts an evaluation failure into a permitting decision.

- **Threshold**: Across the complete set of injected failure conditions — evaluation failure, evaluation cost bound exceeded, the declared backend unavailable, hierarchy provider unavailable, hierarchy provider timeout, malformed or digest-mismatched policy content, unresolvable tenant context, decision records not reaching durable storage within the window of `cpt-cf-policy-engine-nfr-decision-record`, and internal error — zero permitting decisions, of any cause. Every one **MUST** produce a refusal carrying an infrastructure cause. The emergency path of `cpt-cf-policy-engine-fr-emergency-access` is excluded from this threshold and measured separately, since permitting under a failed evaluation is precisely what it exists to do.
- **Rationale**: The gear is the decision authority for operations that change infrastructure. A permissive failure mode converts any gear outage into an admission bypass. This requirement makes the property testable rather than incidental, and it is the reason `cpt-cf-policy-engine-fr-denial-versus-failure` must keep the two reportable apart: failing closed and reporting honestly are different obligations, and satisfying one must not be taken to satisfy the other.
- **Verification Method**: Fault injection across the enumerated failure conditions.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Availability

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-nfr-availability`

The system **MUST NOT** reduce the availability of the process it runs in, and where it runs as a separate process **MUST** meet an availability target strictly better than that of the consumers whose operations it gates.

- **Threshold**: Conditioned on deployment shape, because the platform composes gears both ways and only one of them makes an independent target meaningful.
  - **In-process**, the platform's default composition and the shape of first release: the gear's availability is the host's, so no separate figure applies. The measurable requirement is that no failure mode originating in the gear terminates, stalls, or exhausts its host — verified as zero host-fatal outcomes across the fault-injection set of `cpt-cf-policy-engine-nfr-fail-closed`, together with bounded memory under the content limits of `cpt-cf-policy-engine-fr-operational-limits` and bounded time under the evaluation bounds of `cpt-cf-policy-engine-fr-evaluation-cost-bounds`.
  - **Out-of-process**, where the gear is deployed as its own service: 99.95 percent measured monthly over continuous operation, stated per surface, because the two fail independently there. Decision availability is measured at the decision client, counting any evaluation that returns neither a decision nor a distinguishable infrastructure refusal; management availability is measured at the REST surface. Planned maintenance is excluded only when announced in advance, and the cap differs by surface: 4 hours per month for the management surface, and 30 minutes per month for the decision surface. The decision figure is deliberately close to the error budget the target already allows, because excluding four hours from a 99.95 percent target would concede eleven times the budget the target claims to hold, and because the gear fails closed a decision-surface maintenance window is an outage of every gated operation in the consuming gear and **MUST** be scheduled as one.
- **Rationale**: Because the gear fails closed, its unavailability presents to users as refusal of every gated operation in the consuming gear rather than as a degraded feature. That is what justifies a target better than the consumer's — Infrastructure Resource Manager holds 99.9 percent, and a dependency in its admission path that merely matched it could not leave the consumer room to reach its own figure, since independent outages compound. The argument depends on independence, and co-located code has none: in-process, the gear and its consumer fail together and a separate figure would be arithmetic about a single event. Splitting the requirement keeps the out-of-process target honest and gives the in-process case a property that can actually be failed.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Decision Record Completeness

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-nfr-decision-record`

The system **MUST** produce a decision record for every evaluation, on both permitting and prohibiting paths, carrying enough context to reconstruct the decision without access to the original request.

- **Threshold**: One record per evaluation, on both paths, with no sampling, at the sustained throughput in the scalability requirement. Record content is defined by `cpt-cf-policy-engine-fr-decision-records` and is not restated here. Emission **MUST NOT** extend decision latency, so records are durable asynchronously; at most 5 seconds of decisions may be awaiting durability at any moment, and that window **MUST** be observable. Where the system cannot bring a new decision inside that window — because records are not reaching durable storage — it **MUST** stop returning permitting decisions and refuse with an infrastructure cause until it can. Because that cause is transient and callers retry transients, the system **MUST** signal the condition to callers as one to back off from rather than retry immediately, and **MUST NOT** let retry traffic amplify load on the storage that is already failing. The bound is therefore a serving condition and not only a loss allowance: an unrecordable decision is not made.
- **Rationale**: Auditors must be able to answer why a specific decision was reached months later, and sampling loses exactly the rare decisions most likely to be investigated. The violations projection also reads these records, so a sampled record set would produce a violations list that silently omits refusals. Durability cannot be synchronous without spending the latency budget on it, so the honest requirement is a bounded, measured window. Making that window a serving condition is what stops the bound being decorative: a gear that kept permitting through a long storage outage would produce exactly the unrecorded permits an auditor most needs, and would discover the breach only afterwards. Refusing is consistent with the gear's posture everywhere else — it already refuses when it cannot evaluate, and being unable to record is the same class of failure.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Decision Cache Safety

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-nfr-cache-safety`

Where the system caches anything that contributes to a decision, the cached entry **MUST NOT** outlive the authority it was derived from.

- **Threshold**: No cached entry contributes to a decision for a tenant other than the one it was derived from; no entry remains in use after the policy content version it was derived from stops being active, and in no case beyond the activation propagation window; no failed evaluation is served from cache.
- **Rationale**: A cache that disregards tenant context lets one tenant's decision answer another's request, and one that outlives the content version it came from keeps withdrawn policy in force past the propagation window `cpt-cf-policy-engine-nfr-activation-propagation` promises. Serving a failed evaluation from cache would convert a transient dependency failure into a sustained refusal.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Activation Propagation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-nfr-activation-propagation`

The system **MUST** make an activated or deprecated bundle effective within a bounded and documented window across all evaluation paths.

- **Threshold**: A bundle state change is reflected in decisions within 60 seconds; the window is documented and observable.
- **Rationale**: Operators withdrawing policy during an incident need a known upper bound on when the change takes effect. An unbounded window makes withdrawal unverifiable. The window runs in the other direction too, and that direction is the one that surprises callers: it is equally the interval in which a newly *granted* permissiveness is not yet in force, so a caller that changes policy and immediately performs the operation the change was meant to allow is refused by the policy still in effect. Withdrawal tolerates the window because the old rule refusing for another minute is safe; a grant does not, because the new rule permitting a minute late is a broken workflow. A caller in that position either waits on the observable propagation delay or supplies the deciding fact on the request, per Section 11.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Content Durability

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-nfr-durability`

The system **MUST** be recoverable to a recent consistent state after storage loss, for policy content and its version history.

- **Threshold**: Recovery point within 1 hour and recovery time within 1 hour for policy content, versions, and assignments, verified by a restore exercise rather than by configuration review.
- **Rationale**: Loss of policy content does not degrade the gear, it disarms it: with no content every decision becomes an ungoverned permit, so every guardrail a tenant relied on stops applying and operations that should be refused proceed. The ungoverned count makes that visible, but visible is not the same as prevented. The recovery time is deliberately tighter than the 4 hours Infrastructure Resource Manager allows itself, because the consumer's own recovery assumes its dependencies are already serving. The gear is also the only holder of the version history that audit depends on, and that history cannot be reconstructed from any other system. Decision records are covered by their own retention requirement and are not in scope here.
- **Verification Method**: Restore exercise from backup into a clean deployment.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

### 6.3 Performance

#### Decision Latency

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-nfr-decision-latency`

The system **MUST** return a decision within a bounded time at steady-state load, measured at the gear boundary and including policy matching, expression evaluation, and outcome combination.

- **Threshold**: p95 within 25 ms and p99 within 50 ms for a single evaluation, measured at the gear boundary over **all** evaluations, including those whose hierarchy lookup misses cache. A batch of up to the configured batch bound completes within 100 ms at p95. Decision record emission is excluded, being asynchronous and off the response path. The miss path is not excluded, and it is the binding constraint rather than a tail case: at the hit rate `cpt-cf-policy-engine-nfr-hierarchy-latency` requires, up to one evaluation in ten misses, so the miss population falls inside p95 and the p95 figure must be met **by** the miss path. A miss is bounded at 20 milliseconds by `cpt-cf-policy-engine-fr-dependency-timeouts`, which leaves the rest of the evaluation roughly 5 milliseconds on that path — and that residue, not a figure of its own, is what the evaluation cost-bound defaults of `cpt-cf-policy-engine-fr-evaluation-cost-bounds` are set inside, so that a deployment running every documented default cannot breach this threshold. Retrieval of the applicable policy content is inside this budget with no separate allowance, so any content read that reaches storage on the decision path breaks the target rather than degrading it. The reference load is 50 evaluations per second sustained, over 1,000 tenants averaging 20 active policy documents each; that figure is provisional and its ratification against Infrastructure Resource Manager's load profile is tracked in Section 13.
- **Rationale**: The budget is an allocation out of the enforcing gear's, not an independent target, and it is shared with the gateway that sits between them. Infrastructure Resource Manager holds 500 ms at p95 for a mutation acknowledgment, and admission is one step inside that path alongside validation, classification, and persistence — so a decision consuming more than a small fraction of the consumer's budget would show up as a regression in the consumer's own requirement. Stating the conditioning as unvalidated matches how the consumer states its own latency threshold, rather than inventing a load profile neither document has agreed.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Hierarchy Resolution Latency

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-nfr-hierarchy-latency`

The system **MUST** resolve tenant hierarchy within a bounded time and **MUST** sustain a cache hit rate high enough to keep hierarchy resolution off the critical path for most evaluations.

- **Threshold**: p95 within 2 ms on a cache hit; cache hit rate at or above 90 percent after warm-up under steady-state load.
- **Rationale**: Assignment resolution requires hierarchy, and it sits inside the decision latency budget rather than beside it. A cache hit is an in-process lookup, so it must consume a fraction of that budget; without caching, each evaluation would add a Policy Information Point round trip and the decision target would depend on a dependency with its own budget.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Scalability

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-nfr-scalability`

The system **MUST** absorb load growth and concurrency without violating the decision latency target or failing requests through internal contention.

- **Threshold**: A tenfold increase over the reference load stated in `cpt-cf-policy-engine-nfr-decision-latency` stays within the decision latency target. Separately, and not implied by that rate, 1,000 concurrent in-flight evaluations complete with zero failures attributable to contention: this is a burst and isolation property, since the steady-state rate at the stated latency implies only a few tens in flight, and the number exists to bound what a synchronised arrival does to shared structures rather than to describe normal operation.
- **Rationale**: Evaluation load scales with the total change rate across every gear that gates on policy, not with any single gear's usage.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

### 6.4 Security

#### Tenant Isolation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-nfr-tenant-isolation`

The system **MUST** enforce the content visibility and management boundary defined by `cpt-cf-policy-engine-fr-content-isolation` and `cpt-cf-policy-engine-fr-admin-authorization`, and **MUST NOT** allow a decision for one tenant to be influenced by policy content the requesting context is not entitled to.

- **Threshold**: Zero cross-tenant reads, writes, or decision influences across the isolation test suite, including hierarchy traversal at barrier boundaries and the violations projection.
- **Rationale**: Policy content reveals the security posture of its owner. Leakage across tenants is a confidentiality breach independent of whether any decision changes. The violations projection is named explicitly because it is a read path over records from many tenants and is therefore the easiest place to leak.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

### 6.5 Versatility

No gear-specific versatility threshold applies, because what varies here is structural rather than measurable. The gear runs in either deployment shape described in Section 3.1 with unchanged contracts, and a deployment changes the language policy is written in by changing an evaluation backend rather than by changing this gear, per `cpt-cf-policy-engine-fr-expression-evaluation`.

### 6.6 NFR Exclusions

- Cost and resource-efficiency targets: not modelled per gear in this repository, so no efficiency threshold appears in Section 6.1. What bounds resource use there is functional rather than economic — the operational limits and the evaluation cost bounds — and neither is expressed as a cost figure.
- Data residency and geographic partitioning: policy content follows the deployment's existing residency posture; the gear introduces no separate residency surface.
- Offline and air-gapped operation: not required at first release. The evaluation facility is expected to be in-process, so the gear's only remote dependency is the hierarchy provider, which shares the deployment's connectivity.
- Functional safety and hazard analysis: not applicable. The gear is an information system with no physical actuation and no safety-critical control path.
- Accessibility, internationalisation, and device support: not applicable at first release. The gear exposes no end-user interface; a policy authoring interface is out of scope, and the management API is consumed by operators and other gears.
- Support tiering and diagnostic service levels: not specified here. The gear inherits the platform's support model; the availability requirement above is the only gear-specific service level.
- Operator documentation: required, and inherited from the platform's documentation model rather than specified here — with one gear-specific exception. Because a misconfigured limit, an unpropagated withdrawal, or an unreachable bootstrap path refuses every gated operation across every consuming gear, the configuration surface, the operational limits, and the bootstrap path of `cpt-cf-policy-engine-fr-bootstrap` **MUST** be documented for operators before first release. End-user and training documentation are not applicable: the gear exposes no end-user interface.
- Regulatory certification and data-subject rights beyond erasure: not gear-specific. The gear holds no payment, health, or financial-reporting data, so no scheme-specific regime attaches to it. Personal data is confined to the decision record and governed by `cpt-cf-policy-engine-fr-subject-data-handling` and `cpt-cf-policy-engine-fr-record-retention`; consent, data-subject access, and portability are platform-level obligations discharged where subject identity is owned, not here.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### Policy Decision Client

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-interface-decision-client`

- **Type**: Rust trait, asynchronous, registered in ClientHub without scope
- **Stability**: stable
- **Description**: The decision surface, consumed by the admission gateway on behalf of the gear being gated. Takes the caller's propagated security context together with the evaluation input defined in `cpt-cf-policy-engine-fr-evaluation-input` — which carries the caller-supplied correlation identifier this gear does not mint — singly or as a batch — the context is a required parameter rather than an ambient value, because `cpt-cf-policy-engine-fr-subject-authenticity` derives the subject and its tenant from it — and returns a result that is two-valued in practice — permit or prohibit — with the cause of a permission and the reason for a prohibition. The result type reserves a place for the obligations of `cpt-cf-policy-engine-fr-obligations` and for the deferral of `cpt-cf-policy-engine-fr-deferral-outcome`; until those requirements ship the collection is always empty and the deferral is never returned, so a caller that handles permit and prohibit alone is conformant. Both are reserved rather than added later for the same reason: this contract is stable, and widening a shape callers already match on is a major version. The permission cause is carried on the result rather than as a third variant, so a caller that only branches on permit or prohibit is correct by default and a caller that cares whether policy was silent can ask.
- **Conformance expectations on the caller**: two, neither observable by this gear and both stated here rather than as requirements. Any error result is a refusal, and a caller that treats it otherwise defeats the fail-closed property. An obligation whose identifier the caller does not recognise makes the decision prohibiting; ignoring it silently enforces a permission the policy did not grant.
- **Breaking Change Policy**: Major version bump required.

#### Policy Management Client

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-interface-management-client`

- **Type**: Rust trait, asynchronous, registered in ClientHub without scope
- **Stability**: stable
- **Description**: The administration surface for policy content. Covers bundle, version, document, and target lifecycle, tenant assignment, validation of content before activation, and the decision and violation queries of `cpt-cf-policy-engine-fr-violations`. Separated from the decision surface so that consumers in the admission path do not depend on the administration contract.
- **Breaking Change Policy**: Major version bump required.

#### Policy Administration REST API

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-interface-rest-api`

- **Type**: REST API, versioned, served beneath the platform API prefix
- **Stability**: stable
- **Description**: External administration surface mirroring the management client: policy content, decisions, and violations. Uses the canonical problem error envelope, precondition headers for concurrency control, and the platform's OData query and cursor pagination conventions on list surfaces. Described by a generated OpenAPI document, and subject to the platform's ingress rate limiting rather than a limiter of its own. This is the surface that makes the gear operable without a consuming gear, which is why it is p1 rather than deferred.
- **Breaking Change Policy**: Backward compatible within a major version.

### 7.2 External Integration Contracts

#### GTS Registration

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-contract-gts`

- **Direction**: provided to the types registry
- **Protocol/Format**: GTS link-time inventory in one direction, resolution in the other. The gear registers the policy resource types it exposes for management and its error type family; it resolves, against the registry, the evaluation backend each document declares, the concrete resource types its targets name, and the obligation identifiers its decisions carry.
- **Compatibility**: Type identifiers are stable; new versions are new identifiers.

#### Hierarchy Read Contract

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-contract-hierarchy-read`

- **Direction**: required from `tenant-resolver`
- **Protocol/Format**: In-process client trait via ClientHub. Requires tenant ancestry and descendants with barrier and status handling.
- **Compatibility**: The gear depends on the read surface only and tolerates additive change.

#### Decision Record Contract

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-contract-decision-record`

- **Direction**: provided to downstream consumers
- **Protocol/Format**: Structured records with the stable field set of `cpt-cf-policy-engine-fr-decision-records`, emitted per evaluation.
- **Compatibility**: Additive field changes only within a major version; consumers must tolerate unknown fields.

#### Admission Gateway Engine Contract

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-contract-admission-engine`

- **Direction**: provided to `admission-control`
- **Protocol/Format**: The engine-facing trait the gateway defines, implemented by this gear so the gateway can attach it as one of its engines. The gateway owns the contract; this gear conforms to it without changing its decision semantics. This is the only path by which an evaluation reaches this gear, which is why it is p1 rather than an eventual convenience.
- **Compatibility**: Governed by the gateway's SDK version. The contract is specified — `cpt-cf-admission-control-interface-engine-plugin`, in the gateway's own [PRD](../../admission-control/docs/PRD.md) and [DESIGN](../../admission-control/docs/DESIGN.md) — and is declared unstable there, so its shape may still move under a minor version of that SDK. What does not exist is an implementation on either side. The decision surface it wraps is specified here and is not conditioned on it.

## 8. Use Cases

#### Author and Activate a Policy Bundle

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-usecase-activate-bundle`

**Actor**: `cpt-cf-policy-engine-actor-policy-author`

**Preconditions**:
- The author is entitled to manage policy at the target tenant.

**Main Flow**:
1. The author creates a draft bundle and adds policy documents and targets to it.
2. The author submits the draft for validation.
3. The system validates expression syntax, target vocabularies, and limits, and reports any errors without activating.
4. The author activates the bundle and assigns it to a tenant.
5. The system records content integrity, freezes the bundle against further modification, and begins including it in evaluation.

**Postconditions**:
- The bundle is active, immutable, and effective at the assigned tenant and its descendants within the activation propagation window.

**Alternative Flows**:
- **Validation fails**: The bundle remains in draft; errors identify the offending documents; nothing changes for evaluation.
- **Concurrent modification**: The activation is rejected on the precondition check and the author re-reads before retrying.

#### Admit a Resource Operation

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-usecase-admit-operation`

**Actor**: `cpt-cf-policy-engine-actor-admission-gateway`

**Preconditions**:
- An enforcing gear is about to perform an operation and has passed it to the gateway with the resource type, the resource properties, and the security context of the initiating caller.

**Main Flow**:
1. The gateway submits the evaluation input for the operation the enforcing gear is about to perform.
2. The system resolves the applicable policy set for the resource's tenant, honouring inheritance and precedence.
3. The system evaluates the matching documents and combines their outcomes into one result.
4. The system returns the decision, with a refusal reason naming the responsible policy when negative.
5. The system emits a decision record.

**Postconditions**:
- The gateway relays the decision, the enforcing gear proceeds or abandons the operation, and the evaluation is recorded.

**Alternative Flows**:
- **No applicable policy**: The system permits, marks the permission ungoverned, and records it as such — policy did not approve the operation, it was silent about it.
- **Hierarchy provider unreachable**: The system reports an infrastructure failure distinct from a refusal, so it can be retried as transient rather than surfacing a false policy refusal.

#### Admit a Multi-Type Change

- [ ] `p1` - **ID**: `cpt-cf-policy-engine-usecase-admit-batch`

**Actor**: `cpt-cf-policy-engine-actor-admission-gateway`

**Preconditions**:
- An enforcing gear has classified a change touching several resource types and needs one verdict, relayed through the gateway, before dispatching any work.

**Main Flow**:
1. The gateway submits one batch containing an evaluation input per resource type the change touches.
2. The system evaluates each member and records each independently.
3. The system combines the member outcomes into one verdict, refusing the batch if any member is refused.
4. The system returns the verdict together with the per-member outcomes.

**Postconditions**:
- The enforcing gear either dispatches the whole change or refuses it whole, naming every member that was refused.

**Alternative Flows**:
- **Batch exceeds the configured bound**: The system fails the request against the limit rather than evaluating a subset.

#### Review Recent Refusals

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-usecase-review-violations`

**Actor**: `cpt-cf-policy-engine-actor-tenant-policy-admin`

**Preconditions**:
- Policy is active for the tenant and at least one operation has been refused within the retention window.

**Main Flow**:
1. The administrator queries violations for their tenant, filtered by policy document, resource type, or time range.
2. The system returns the prohibiting decisions within the retention window that the administrator is entitled to see.
3. The administrator inspects an entry to identify the policy that refused and the operation that was attempted.

**Postconditions**:
- The administrator can either correct the operation or revise the policy, without reading gear logs.

**Alternative Flows**:
- **Query spans tenants the administrator does not manage**: The system returns only the entitled subset, indistinguishably from those entries not existing.
- **Retention window has elapsed**: Refusals older than the window are absent, and the window in force is reported alongside the result.

#### Withdraw an Active Bundle

- [ ] `p2` - **ID**: `cpt-cf-policy-engine-usecase-withdraw-bundle`

**Actor**: `cpt-cf-policy-engine-actor-platform-operator`

**Preconditions**:
- A bundle is active and an operator has determined it must stop taking effect.

**Main Flow**:
1. The operator deprecates the bundle or removes its tenant assignment.
2. The system stops including it in evaluation within the activation propagation window.
3. The system records the change against the operator.

**Postconditions**:
- Decisions no longer reflect the withdrawn bundle; the bundle and its history remain available for audit.

**Alternative Flows**:
- **Withdrawal would leave no applicable policy**: Evaluation falls back to ungoverned permits, and the system makes that consequence visible before the change is applied, since an operator withdrawing a guardrail needs to know the operations it covered will start proceeding rather than start failing.

## 9. Acceptance Criteria

- [ ] A management gear can gate every operation it declares as policy-gated against this gear, with no policy rules remaining in its own source.
- [ ] A deployment with no policy content admits operations rather than refusing them, and every such permission is marked ungoverned and counted.
- [ ] The result of an evaluation is unchanged when the applicable set is presented in any order.
- [ ] A change touching several resource types produces one verdict that names every refused type, in a single request.
- [ ] Every enumerated failure condition produces a refusal carrying an infrastructure cause, and no configuration produces a permission of any cause on failure.
- [ ] A refusal caused by an unreachable dependency is distinguishable, in both the response and the metrics, from a refusal caused by policy.
- [ ] A hung hierarchy provider produces a refusal within the stated timeout rather than stalling the request.
- [ ] An expression that exceeds the configured cost bound refuses its evaluation without affecting the latency observed by concurrent evaluations.
- [ ] Policy content can be authored, validated, activated, and withdrawn without a deployment restart, and every state change is attributable to an actor.
- [ ] An activated or withdrawn bundle takes effect within the activation propagation window, verified by measurement rather than by inspection.
- [ ] An administrator can retrieve every refusal for their tenant within the retention window, without access to gear logs.
- [ ] An auditor can reconstruct any past decision from its record, including which policy version determined it and how many documents were matched versus evaluated.
- [ ] Policy content and its version history survive a restore exercise into a clean deployment within the stated recovery objectives.
- [ ] Policy content, decisions, and violations are confined to their owning tenant across the isolation test suite, including at barrier boundaries.
- [ ] An evaluation whose asserted subject or subject tenant disagrees with the caller's propagated security context is refused with its own cause, and no decision is returned and no record attributes the evaluation to the asserted identity.
- [ ] An ancestor can neither assign policy at, nor read the records of, a tenant behind a self-managed barrier, while an assignment it holds above that barrier still governs inside it under the barrier handling the caller requests.
- [ ] A tenant administrator reads a refusal produced by an ancestor's document, receives that document's identity, and cannot read its content.
- [ ] Reading decision records and reading the violations projection are separately authorised, neither is implied by the ability to read policy content, and a read performed under the cross-barrier capability leaves an administration event.
- [ ] No management operation allows a caller to grant an entitlement it does not itself hold.
- [ ] The evaluation input carries the subject's identity and no entitlement of it, so no policy document can decide by role, permission, or group membership, and no decision the gear returns is expressible as an authorization grant.
- [ ] A deployment with no policy content can create its first bundle without an undocumented path.
- [ ] Policy content cannot reach the network or the filesystem, and two evaluations of identical input produce identical outcomes.

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| Policy evaluation facility | Carries the evaluation backends and the contract they implement, and interprets document content in the language each declares. Does not exist yet, in any form, anywhere in the repository. No evaluation path exists without it, so this gear cannot ship before it does | p1 |
| `tenant-resolver` | Tenant ancestry, descendants, barrier and status handling, used in assignment resolution | p1 |
| `toolkit-db` | Persistence for policy content and decision records, with scoped access through the secure data layer | p1 |
| `types-registry` | GTS registration, and both directions of registry-reference resolution: forward at write time for the identifiers this gear persists as references rather than as strings, and reverse at read and export time so every surface still speaks GTS identifiers. Covers the resource-type identifiers policy targets match, the backend each document declares, and the resource type each decision record carries | p1 |
| `authz-resolver` | Authorizes this gear's own management operations. The gear is a Policy Enforcement Point for its administration surface only: it builds an access request per management capability and consumes the decision. It neither provides decisions to this path nor evaluates authorization policy | p1 |
| Emergency entitlement source | Resolves whether a subject holds the emergency entitlement, without consulting policy content; no component provides this today | p3 |
| `event-broker` | Transport, retention, and export of decision records, published to a platform-scoped audit topic shared with `admission-control`. Not on the durability path: the gear's own record table is where a decision becomes durable, and `cpt-cf-policy-engine-nfr-decision-record` is met against that table rather than against delivery. What the topic adds is retention beyond this gear's window and a single stream an auditor can read. The topic is scoped by the resource tenant of `cpt-cf-policy-engine-fr-decision-records`, which is not always the subject's tenant, so the tenant a record is published under is the tenant it is scoped and filtered by | p2 |

## 11. Assumptions

- Subjects arrive already authenticated, with identity and tenant context established upstream by `authn-resolver`.
- The authorization path is present and has a decision point behind it. The gear authorizes its own management operations through it and cannot authorize them itself: policy content deciding who may write policy content would make the administration surface self-granting, which `cpt-cf-policy-engine-fr-admin-authorization` forbids.
- Authorization is decided before admission on every gated operation, and the caller treats a permission from this gear as policy raising no objection rather than as an access grant. `cpt-cf-policy-engine-fr-authorization-boundary` places this on the caller because the gear cannot observe it: an evaluation request looks identical whether or not authorization ran first, so a consumer that skipped it would be refused nothing and warned of nothing.
- Neither component above this gear is built yet. The `admission-control` gateway, this gear's only consumer, is specified — its PRD and design define the engine plugin contract this gear implements, and mark it unstable — but unbuilt; Infrastructure Resource Manager likewise exists as a specification and not yet as software. Infrastructure Resource Manager's own requirements describe it calling a decision service directly, so routing it through the gateway is a change to that gear's integration which nobody has yet agreed. Its admission requirements are stable enough to design against and are what this gear is measured by, but end-to-end integration validation, and ratification of the reference load in Section 6, wait on that gear being built. No requirement here depends on its implementation.
- Consumers honour the obligations they receive, and refuse rather than proceed on an obligation identifier they do not recognise, as `cpt-cf-policy-engine-interface-decision-client` expects of them. The gear cannot observe whether they do, and no requirement here depends on detecting it. The assumption is safe to carry unverified while `cpt-cf-policy-engine-fr-obligations` remains p3, because nothing emits an obligation until it ships.
- Tenant hierarchy is a single-root tree with barrier semantics as defined by the platform tenant model; the gear reads it and does not reinterpret it.
- The evaluation facility, once it exists, runs in the same process as the gear. This bounds transport failure modes but does not remove them: an in-process evaluation can still exhaust its cost bound, so the isolation and cost requirements apply regardless of co-location, and the hierarchy provider is a separate gear in every deployment.
- Every backend the evaluation facility carries provides two properties this gear depends on and cannot supply itself: syntax validation exposed independently of evaluation, and acceptance of an externally imposed per-document wall-clock bound. Both were confirmed against the first candidate audited, a Rego implementation, which parses a policy to an error or a package name without evaluating it and accepts a caller-set wall-clock limit checked cooperatively between work units. Two consequences of that audit are carried in the requirements rather than assumed away. The bound has a granularity, because it is checked between work units and not preemptively, so a per-document figure is an upper bound plus one work unit and not an instant. And no backend supplies the third property this gear once assumed of it — a declared sandbox guarantee — which is why `cpt-cf-policy-engine-fr-evaluation-isolation` now requires the facility to declare it for a specific backend build and requires this gear to enforce determinism by refusing content.
- Evaluation memory is bounded by the content limits of `cpt-cf-policy-engine-fr-operational-limits` and by nothing else. An allocator-level bound on what one evaluation may allocate is not assumed to exist: the audited candidate offers one only through an allocator that may conflict with the host's, so the in-process clause of `cpt-cf-policy-engine-nfr-availability` rests on bounding the size of what is evaluated rather than on capping allocation during it.
- Consumers enforce the decisions they receive. The gear has no mechanism to detect a caller that requests a decision and ignores it.
- Consumers supply complete operation context. A property that policy needs and the caller omits evaluates as absent, so a policy author's filters and a consumer's context are coupled, and the platform has no mechanism to verify the coupling.
- Tenant entitlements that change over the life of a tenant — a subscription plan, a service tier, a contract state — reach policy as operation-context properties supplied on the request, not as assignment structure the gear has to be reconfigured to reflect. This keeps an entitlement change out of `cpt-cf-policy-engine-nfr-activation-propagation` entirely: nothing about the policy changes, so nothing has to propagate, and the next request carries the new fact. Two costs come with it and neither is verifiable here. The gear cannot check a fact the caller relays but does not own — a subscription plan belongs to whatever system sells subscriptions, and the caller is repeating it — so the trust in that value is the trust in the caller and in the source the caller read it from, which Section 13 records as unresolved. And `cpt-cf-policy-engine-fr-record-confidentiality` keeps caller-supplied values out of the record, so a decision that turned on a relayed entitlement is attributable to the property name and not to the value it held; the record shows that the plan was read, never which plan it was. That is the same limit `cpt-cf-policy-engine-fr-version-comparison` already records for replaying a decision, and it bounds what the reconstruction promised by `cpt-cf-policy-engine-nfr-decision-record` can recover for this class of decision.
- Policy content volume per tenant is small relative to resource volume, so policy retrieval is cacheable and does not dominate evaluation cost.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| The evaluation facility does not land | No evaluation path exists at all, and the gear cannot ship | Nothing this gear can mitigate; the dependency is a prerequisite, and Section 10 records it as such |
| A backend upgrade adds a non-deterministic builtin, and the denylist of `cpt-cf-policy-engine-fr-evaluation-isolation` goes stale | Determinism is lost silently. Nothing fails, no content is rejected, and the loss shows up only as two decision records that disagree about the same input — which is the hardest defect in this gear to attribute, because the policy, the version and the digest all match | Bind the denylist and the sandbox declaration to a specific backend build rather than to a backend, and make re-auditing the builtin set a required step of upgrading it. Treat an unaudited build as an undeclared one, which `cpt-cf-policy-engine-fr-evaluation-isolation` already refuses to select |
| The facility carries only one backend for longer than expected, making the language a de facto platform commitment | Authors cannot express the rules they need, and pressure returns to add a gear-level engine registry after the contract has consumers | Validate the first backend's language against the guardrail set Infrastructure Resource Manager actually needs before DESIGN; keep the language declaration on every document from first release, so a second backend is an addition rather than a content migration |
| Gear latency or unavailability propagates to every gated operation | The consuming gear cannot meet its own latency and availability requirements, since the two compound | Hold the latency and availability targets as release gates; measure at the gear boundary; keep hierarchy caching on the critical path |
| Caller-supplied context and policy filters drift apart | Policy silently stops matching, and a guardrail that appears active refuses nothing | Make the applicable-set and matched-versus-evaluated counts observable per decision; treat a policy that never matches as an alertable condition rather than a silent one |
| Policy inheritance behaves wrongly at self-managed tenant boundaries | An ancestor guardrail silently stops applying, or a delegated tenant's policy reaches beyond its boundary; both are invisible until a decision is questioned | Make barrier behaviour explicit per assignment rather than a global default; treat crossings as a test surface, not a configuration detail |
| Policy content growth makes evaluation cost scale with tenant count | Latency target becomes unreachable at large deployments | Bound bundle size, document count, applicable-set size, and batch size; measure evaluation cost against tenant and document count, not only against request rate |
| The gear accumulates policy responsibilities faster than consumers adopt it | Specified surface with no users, and requirements shaped by speculation rather than integration | Keep the document-kind set closed and narrow; admit a new kind or a new consumer only with a concrete integration behind it |
| Neither component above this gear exists yet: the admission gateway is unbuilt and Infrastructure Resource Manager is a specification | This gear has no path to a real consumer and cannot be integration-validated until two other gears land, so defects in its contract surface late | Keep the decision surface specified independently of the gateway contract, so the gear can be exercised through a harness standing in for the gateway; treat the gateway's contract shape as a release gate rather than an implementation detail discovered during integration |
| A tenant entitlement is modelled as assignment structure rather than as request input | The propagation window becomes an ordering hazard rather than a withdrawal bound: an upgrade that swaps assignments and immediately provisions is refused by the policy it has just replaced, and the refusal is correct, attributable to nothing the operator changed, and invisible until someone times it | Express a changing entitlement as an operation-context property so no propagation is involved; where the structure is genuinely the right model, order the entitlement change before the operations that depend on it and gate the sequence on the exported propagation delay rather than on a fixed sleep |
| Violations derived from records prove insufficient for what administrators actually want | The projection is delivered, unused, and a standing-compliance capability is requested instead | Validate the projection against a real administrator query set before DESIGN; treat a request for standing state as a scope change requiring its own requirement, not an extension of this one |

## 13. Open Questions

Each question carries an owner role and the point by which it must be answered. A question unanswered past its point blocks the artefact named beside it.

| Question | Owner | Needed by |
|---|---|---|
| What re-audits the sandbox-and-determinism declaration of `cpt-cf-policy-engine-fr-evaluation-isolation` when a build changes, and does the declaration carry enough for this gear to detect a stale one? DESIGN Section 3.4 places authorship with the evaluation facility's SDK, which settles where the claim is made and not what keeps it true. The claim is about a **build** — which compile-time features are enabled and which builtins the result registers — so it goes stale on a dependency bump or a feature change. The trigger that re-audits it is the facility's own to define and belongs in the facility's documentation, which Section 4.2 keeps outside this gear and which is not yet written. What this gear needs from that answer is the declaration's field set: it can presently refuse an absent declaration but not a stale one, and a schema two gears read cannot gain fields once implementation has begun, which is why the answer is needed before implementation rather than before first release. | Evaluation facility owner | Before implementation |
| Where do the schemas that both this gear and `admission-control` read belong: the declaration a backend build carries — its two backend guarantees together with the sandbox-and-determinism claim `cpt-cf-policy-engine-fr-evaluation-isolation` refuses to select a build without — and the obligation base type whose instances travel from a decision point through the gateway to the enforcing gear? DESIGN Section 3.4 assumes the first belongs to the evaluation facility's SDK and the second to `libs/toolkit-gts`, and the gateway's design assumes the same; neither gear is entitled to decide it, because both read both, so a schema owned by either would make the other depend on it. An answer changes only where the two gears import from, which is why it is needed before implementation rather than before first release. | Platform architecture | Before implementation |
| Is an allocator-level memory bound available in any facility build, or is evaluation memory bounded only by content size? The audited candidate offers one solely through an allocator that may conflict with the host's, which would leave `cpt-cf-policy-engine-nfr-availability` resting on content limits alone. | Gear owner | Before first release |
| Should an assignment be inheritable across a self-managed tenant barrier, and under what caller conditions? The platform treats barriers as context-dependent, so a single default is wrong for either operator guardrails or delegated policy. This is the evaluation plane only: the management plane is settled by `cpt-cf-policy-engine-fr-assignment-authorization`, and the record plane by `cpt-cf-policy-engine-fr-record-visibility`, both of which respect barriers and neither of which depends on how this is answered. | Platform architecture | Before first release |
| Does any enforcing gear need a decision for a subject other than the one in its own security context — work queued, scheduled, or retried after the requesting subject's context is gone — and is the service identity the platform's two-plane decision prescribes for that case sufficient? Today it has to be: the security context carries no actor field, so one subject acting for another is not expressible, and an on-behalf-of mode would need that field plus an authority able to sanction it. `cpt-cf-policy-engine-fr-subject-authenticity` refuses the case deliberately, which makes a real need for it a new platform capability rather than a relaxation of this gear's requirement. | Platform architecture | Before first release |
| What are the bounds on bundle size, document count per bundle, applicable-set size, and batch size? These are user-visible limits that shape how policy can be organised. | Gear owner | Before first release |
| Does the reference load in `cpt-cf-policy-engine-nfr-decision-latency` — 50 evaluations per second over 1,000 tenants at 20 documents each — match Infrastructure Resource Manager's real profile? The figure is provisional and every latency and scalability threshold is conditioned on it. | Gear owner | Before first release |
| Who owns the emergency entitlement that `cpt-cf-policy-engine-fr-emergency-access` requires, and does it exist? The requirement needs an entitlement resolvable without consulting policy content, and no component provides one. | Platform architecture | Before the emergency path ships |
| Where do deferred outcomes terminate? The approval service is the natural destination, but nothing connects them and that gear has no implementation. | Gear owner | Before the deferral outcome ships |
| Does the event trigger and the after-the-operation phase have a consumer, or should the declared vocabulary narrow to what first release implements? | Gear owner | Before first release |
| Which component owns the authoritative record of a tenant's subscription plan and entitlement state, and can an enforcing gear read it at request time? The Section 11 assumption that a changing entitlement arrives as request input depends on the caller having a trusted source to read; without one the caller is asserting its own entitlement, and the alternative — modelling the entitlement as assignment structure — pays the propagation window on every transition. | Platform architecture | Before first release |

## 14. Traceability

Downstream artifacts for this gear are partially written. ADRs and features belong alongside this document when they exist.

- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: `ADR/` — not yet created; the records land here once written
- **Features**: `features/` — not yet created
- **Enforcing gear**: [Infrastructure Resource Manager](../../../infrastructure-resource-manager/docs/PRD.md), reached through the gateway, whose admission requirements this gear's policy must be able to express: the pre-create admission pipeline, cascade admission, and policy gating. Its management-policy guardrails and its orphan-capacity rule are excluded — the first is that gear's own per-resource state and the second is a counter, and Section 4.2 places both outside this gear
- **Consumer**: [admission-control](../../admission-control/docs/PRD.md) and its [DESIGN](../../admission-control/docs/DESIGN.md) — the gateway gear specified separately, this gear's only direct consumer, which attaches this gear as one of its engines and owns the plugin contract it conforms to
- **Platform architecture**: [ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md), [GEARS.md](../../../../docs/GEARS.md)
- **Comparable gear**: [CredStore](../../../credstore/docs/PRD.md), for the local-implementation-plus-management pattern this gear follows
- **Existing evaluation subsystems**: [quota-enforcement](../../quota-enforcement/docs/PRD.md) and [event-broker](../../event-broker/docs/PRD.md). This gear follows their engine-boundary discipline — the caller validates what the engine returns — and departs from their pluggable-engine shape, as Section 5.7 records
- **Subject identity and erasure**: [account-management ADR-0005](../../account-management/docs/ADR/0005-cpt-cf-account-management-adr-idp-user-identity-source-of-truth.md), whose position that the identity provider is the sole source of truth for user identity and the sole handler of erasure requests is what `cpt-cf-policy-engine-fr-subject-data-handling` follows
- **Standards lineage**: the PDP, PEP, PAP, and PIP vocabulary derives from NIST SP 800-162
