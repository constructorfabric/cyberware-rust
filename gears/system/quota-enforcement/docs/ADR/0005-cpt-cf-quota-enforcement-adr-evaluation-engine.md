---
status: accepted
date: 2026-05-07
---

# Pluggable evaluation engine — capability-based contract


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Capability contract](#capability-contract)
  - [Reference implementations (non-normative)](#reference-implementations-non-normative)
  - [Operator freedom](#operator-freedom)
  - [Engine errors → CanonicalError](#engine-errors--canonicalerror)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Pluggable trait with capability contract](#pluggable-trait-with-capability-contract)
  - [Pin a specific engine in QE-core](#pin-a-specific-engine-in-qe-core)
  - [Per-engine switch in QE-core](#per-engine-switch-in-qe-core)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-quota-enforcement-adr-evaluation-engine`

## Context and Problem Statement

QE arbitrates multi-Quota admission through an «Engine» — the component that takes the
applicable-Quotas set, the request, and an opaque per-Policy config, and returns a
`Decision { result, debit_plan, diagnostics }`. Different deployments need different
arbitration logic: simple closed-form (every applicable Quota debited equally),
operator-authored rich expressions (CEL / Starlark / Lua), or domain-specific DSLs
(constrained, statically-typed, formally verifiable). Pinning a specific engine in
QE-core would either over-constrain rich-policy deployments or add complexity to small
ones.

QE-core therefore needs to specify **what an Engine must guarantee**, not **which
engine** provides it. The decision this ADR records is *how* engines are selected — not
*which* engine is selected.

## Decision Drivers

- Engine pluggability (`cpt-cf-quota-enforcement-fr-quota-resolution-engine`).
- No QE-core lock-in to a specific engine technology, language, or vendor.
- Determinism for replay-safety (`cpt-cf-quota-enforcement-fr-idempotency`).
- Sandboxing — no I/O, no nondeterminism beyond `EvaluationContext.time`.
- Cost-bounded evaluation — per-Policy timeout is operator-tunable, default 5 ms.
- Pre-compilable for hot-path latency
  (`cpt-cf-quota-enforcement-nfr-evaluation-latency` p95 ≤ 100 ms).
- Strict trust boundary — orchestrator validates every Decision against Debit-Plan
  invariants before applying any counter mutation
  (`cpt-cf-quota-enforcement-principle-strict-engine-boundary`).
- Independent operational evolution — engines evolve along axes orthogonal to storage
  / coordination / notification.

## Considered Options

- **(a) Pluggable trait with capability contract** — QE-core defines a single Rust
  trait (`QuotaResolutionEngineV1`) and a capability list; any engine that satisfies
  the contract is acceptable.
- **(b) Pin a specific engine in QE-core** — hardcode a single engine technology
  (e.g., CEL only, or `most-restrictive-wins` only).
- **(c) Per-engine switch in QE-core** — QE-core natively understands several engines
  and dispatches via a switch.

## Decision Outcome

Chosen option: **(a) — pluggable trait with capability contract.** Engines are
decoupled into `QuotaResolutionEngineV1` (defined in
`cpt-cf-quota-enforcement-fr-quota-resolution-engine`). QE-core specifies only
outcome-based **capability requirements** (DESIGN §3.3 «Engine Plugin Trait»). Any
engine that satisfies the contract is contract-compliant — QE-core does not name,
prefer, or constrain a specific engine technology.

### Capability contract

The Engine plugin contract requires (full surface in DESIGN §3.3):

- **Rust-native, in-process linkage** at gear bootstrap (no out-of-process
  evaluation hot path; no FFI on the eval critical path in P1).
- **Sandboxed**: no I/O, no nondeterminism beyond `EvaluationContext.time`. The
  sandbox is the absence of unsafe bindings — verifiable by code review.
- **Cost-bounded**: per-Policy timeout (operator-configurable, default 5 ms); engine
  internally implements bounding (steps cap / instruction cap / wall-time cap). The
  effective budget reaches the engine as the typed `EvaluationBudget` on `EvaluationContext`.
- **Pre-compilable**: validated `engine_config` is parsed/compiled once at Policy
  create/update and cached by `(policy_id, policy_version)`; cache miss rebuilds.
- **Deterministic** given EvaluationContext — required for idempotency replay
  equivalence (`cpt-cf-quota-enforcement-fr-idempotency`).
- **Closed contract surface**: returns `Decision { result, debit_plan, diagnostics }`
  on success, `EngineError` (closed enum) on failure. The orchestrator validates the
  Decision against Debit-Plan invariants before any mutation.
- **Three-method trait**: `id()`, `validate_config(raw)`, `evaluate(ctx, config)`.

An engine that meets every capability is plugin-compliant. Concrete realisation —
language family, evaluator implementation, AST cache strategy, cost-bounding
mechanism, sandbox enforcement technique — is **engine-internal**.

### Reference implementations (non-normative)

P1 ships two reference engines bundled with QE-core for default-deployment ergonomics:

- **`most-restrictive-wins`** — hardcoded engine, no `engine_config`. Sub-millisecond
  hot path. Default global-Policy engine
  (per `cpt-cf-quota-enforcement-fr-quota-resolution-policy`); rejects any non-empty
  `engine_config`. Hardcoded because this arbitration shape («every applicable Quota
  debited equally») has no operator-authored degrees of freedom.
- **`cel`** — sandboxed CEL evaluator backed by the `cel-interpreter` crate
  (Rust-native, sandbox by construction, cost-bound via
  `Context::with_cost_limit(steps)`, pre-compiled AST cache keyed by
  `(policy_id, policy_version)`). Operators author `cel` Policies via
  `engine_config.expr` (CEL string). The `cel-interpreter` choice is the reference
  realisation — a different Rust-native CEL evaluator (or even a non-CEL expression
  language) could ship as an alternative engine without contract change.

These are **non-normative defaults**. Operators may ship additional engines —
Starlark, Lua, Wasm-loaded operator engines, custom statically-typed DSLs (e.g., the
post-P3 «constrained quota-arbitration DSL» tracked in PRD §13) — implementing
`QuotaResolutionEngineV1`. Engines are linked into the QE binary at build time
(runtime registration of arbitrary user-supplied engines is out of scope per
`cpt-cf-quota-enforcement-constraint-in-process-engine-registration`).

### Operator freedom

Selecting an engine is an **operator-deployment decision**, not a QE-core decision.
The single normative constraint is conformance to the §3.3 capability list. Operators
may:

- use only the bundled engines (most deployments);
- replace either bundled engine for a specific Policy by changing `engine_id`
  (Policy-level decision per `cpt-cf-quota-enforcement-fr-quota-resolution-policy`);
- ship a custom engine crate linked into the QE binary at build time;
- run a hybrid where bundled engines handle most Policies while a custom engine
  handles specialised ones.

QE-core neither requires nor inspects the operator's engine choices — it only
requires that whichever engines are registered conform to the contract.

### Engine errors → CanonicalError

`EngineError` (closed enum: `Timeout`, `CostExceeded`, `TypeError`, `InvalidConfig`,
`Internal`) is lifted into `CanonicalError` per the DESIGN §3.3 mapping table:

- `EngineError::Timeout` → `CanonicalError::DeadlineExceeded` (HTTP 504).
- `EngineError::CostExceeded` → `CanonicalError::ResourceExhausted` (HTTP 429,
  `subject = "engine"`).
- `EngineError::TypeError` / `Internal` → `CanonicalError::Internal` (HTTP 500).
- `EngineError::InvalidConfig` is caught at Policy create/update (delegated via
  `validate_config()`) and never reaches the eval hot path.

This mapping is contract-level and applies to every engine plugin uniformly.

### Consequences

- Adding an engine = link a new crate, register at gear bootstrap (per
  `cpt-cf-quota-enforcement-fr-quota-resolution-engine` fail-fast bootstrap rule),
  no QE-core or sibling-ADR change required.
- Engine config validation is engine-internal (delegated via `validate_config()` at
  Policy create/update; engine returns parsed/validated form or structured error).
- Per-engine telemetry uses `engine_id` label (bounded cardinality per
  `cpt-cf-quota-enforcement-constraint-bounded-cardinality`); per-engine internals
  (e.g., CEL's specific evaluator crate choice, AST representation, cost-cap
  implementation) are plugin-internal.
- Trust boundary preserved: every Decision validated by `EvaluationOrchestrator`
  against the closed Debit-Plan invariant set before mutation.
- Engine deprecation lifecycle (intentional removal of an engine from the deployment
  binary while persisted Policies still reference it) is tracked separately as a
  P2-deferred consideration in DESIGN §4.3 — out of scope for this ADR.

### Confirmation

Confirmed for any engine impl (reference or third-party) by:

- code review against the `QuotaResolutionEngineV1` trait + capability list;
- bench gating per-Policy timeout under
  `cpt-cf-quota-enforcement-nfr-evaluation-latency`
  (`engine_evaluation_seconds` histogram within the configured timeout);
- sandbox audit verifying no I/O reachable from the engine on the hot path;
- replay-equivalence test: identical EvaluationContext → byte-identical Decision.

## Pros and Cons of the Options

### Pluggable trait with capability contract

- Good, because operators get full freedom of engine choice within the capability
  envelope (CEL, Starlark, Lua, Wasm, custom DSLs).
- Good, because it isolates QE-core from specific engine technologies; no leak of
  CEL-isms (or any other engine's quirks) into QE-core code.
- Good, because contract evolution is localized to the trait surface.
- Good, because mirrors the pluggable-storage / pluggable-notification pattern across QE — single architectural
  idiom.
- Good, because trust boundary (Debit-Plan invariants) protects QE-core integrity
  regardless of engine quality.
- Bad, because each engine plugin owner carries the cost of their own conformance
  suite (sandbox audit, bench, replay test).

### Pin a specific engine in QE-core

- Good, because simplest possible mental model.
- Bad, because locks every deployment to one engine; unsuitable for deployments that
  need expressive arbitration (rich CEL Policies) and equally unsuitable for
  deployments that want simpler / faster paths.
- Bad, because engine-specific quirks leak into QE-core, complicating future
  migrations.

### Per-engine switch in QE-core

- Good, because no operator-side authoring required.
- Bad, because complexity explosion as engines accrete; engine-set evolution becomes
  a QE-core release.
- Bad, because adds engines QE-core must understand and test, defeating the
  separation of concerns.

## More Information

The capability-level contract lives in DESIGN §3.3 «Engine Plugin Trait» plus the FR
in PRD §5.9 (`cpt-cf-quota-enforcement-fr-quota-resolution-engine`). The two reference
impls (`most-restrictive-wins` and `cel`) live in their own plugin crates within the
QE workspace; their concrete realisations (AST cache, cost-bounding mechanism,
sandbox technique) are plugin-internal and out of scope for QE-core.

PRD §13 lists related open questions whose resolution will affect this ADR's
operational landscape: P2 engine candidates (Starlark / Lua / Wasm), engine
deprecation lifecycle, constrained quota-arbitration DSL (post-P3).

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses:

- `cpt-cf-quota-enforcement-fr-quota-resolution-engine` — establishes the engine
  plugin trait shape and the capability requirements.
- `cpt-cf-quota-enforcement-fr-idempotency` — engine determinism is the precondition
  for replay equivalence.
- `cpt-cf-quota-enforcement-nfr-evaluation-latency` — capability obligation;
  per-engine realisation varies.
- `cpt-cf-quota-enforcement-principle-strict-engine-boundary` — orchestrator-side
  Debit-Plan invariant validation isolates QE-core from engine bugs.
- Sibling ADR `cpt-cf-quota-enforcement-adr-storage-backend` — same
  pluggable-with-capability-contract pattern applied to storage.
- Sibling ADR `cpt-cf-quota-enforcement-adr-coordination-plugin`: singleton coordination is consumed from the
  platform `cluster` gear instead of a QE plugin.
