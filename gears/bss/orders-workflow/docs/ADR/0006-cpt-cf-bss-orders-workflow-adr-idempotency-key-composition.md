---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team (Architecture)
---

# ADR-0006: Provisioning-Intent Idempotency Keys Are Composed From Order, Version, Line, Wave, and Intent Kind — Never Reused as the Correlation ID

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Five-part composite key (chosen)](#five-part-composite-key-chosen)
  - [Four-part key omitting `intentKind`](#four-part-key-omitting-intentkind)
  - [Three-part key omitting `orderVersion`](#three-part-key-omitting-orderversion)
  - [Single shared identifier for both idempotency and correlation](#single-shared-identifier-for-both-idempotency-and-correlation)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-workflow-adr-idempotency-key-composition`

## Context and Problem Statement

PRD §6.3 requires every provisioning intent — draft-create, activation, draft-void, activated-cancel — to carry both a process `correlationId` and a separate idempotency key derived from `orderId`, `orderVersion`, order-line reference, and wave, explicitly forbidding the key from omitting `orderVersion` and from being reused as the `correlationId`. Those four components are a floor, not a ceiling: they do not by themselves separate a compensating submit from the forward submit it undoes, nor one rebuild attempt from another, nor one tenant's key space from another's. The caller-side duplicate-submission protocol (retry-then-lookup on client-side timeout, wait-then-retry-same-key on conflict) depends on that key being stable across retries of the *same* logical submit, yet distinct across every submit that is not logically the same — including a later version of the same line and wave, which the order's amendment path (Lifecycle) can produce while an earlier version's fulfillment is still in flight or being compensated. Approval requests (§6.2/§6.4 gates) have an analogous but narrower duplicate-submission problem. How should the idempotency key be composed so that retries of one logical submit collapse safely, a superseded version of the same line/wave never collides with it, and the key can never be mistaken for the correlation identifier that ties an intent's confirmation back to its process instance?

## Decision Drivers

* Every retry of one logical submit — including the client-side-timeout retry-with-same-key path — must produce the identical key, or the caller-side duplicate protocol cannot detect its own retries as duplicates.
* A later `orderVersion` for the same line and wave is a distinct logical submit (the earlier one may be superseded, cancelled, or already compensated); if the key does not include `orderVersion`, Subscriptions cannot tell the two apart and may either reject the newer submit as an in-flight duplicate of the old one or, worse, silently no-op it.
* Wave must be part of the key because draft-create and activation for the same line are two logically distinct submits that must not collide with each other.
* The compensating actions (draft-void, activated-cancel) run at the *same* wave, against the *same* line, under the *same* order version as the forward intent they undo. Order, version, line and wave therefore cannot distinguish them: a four-component key makes a `draft_void` byte-identical to the `draft_create` it reverses, so `UNIQUE (idempotency_key)` rejects the compensating row locally and Subscriptions returns the stored forward outcome as an absorbed duplicate — compensation records success against a live subscription. The direction of the submit must itself be a key component.
* A wave-1 rebuild (re-running wave 1 after a draft was auto-voided upstream) is a genuinely new logical submit of an intent whose order, version, line, wave and kind are all unchanged; it needs one component that is permitted to change, or the rebuild is unexpressible under a deterministic composition.
* Keys are minted per tenant and must not be comparable across tenants; a shared key space would let one tenant's submission collide with another's.
* The `correlationId` exists to let confirmations and failure events be traced back to the process instance across the whole intent lifecycle (including reconciliation-sweep lookups); collapsing it with the idempotency key would mean a retried submit, which must keep the same idempotency key, could never carry a fresh correlation identity, and conversely a key rotation for correlation purposes would break duplicate detection.
* The sweep (§6.3 Intent Reconciliation Sweep) looks up intents by `correlationId` plus `orderId` + `orderVersion` + line reference + wave, or by the recorded transition-request identifier — this lookup shape only works if the idempotency key's components are independently addressable fields, not an opaque hash the sweep cannot decompose.
* Approval requests have their own duplicate-submission problem (a gate re-evaluated after a retry or after `OrderAcceptanceRecorded`) but at order-and-gate granularity, not line-and-wave granularity, so their key needs a distinct, narrower composition rather than reusing the provisioning-intent shape wholesale.

## Considered Options

* **Five-part composite key** (`orderId`, `orderVersion`, `orderLineId`, `wave`, `intentKind`), tenant-prefixed and kept structurally distinct from `correlationId`; sibling key families for approval requests and Lifecycle transition calls
* **Four-part key omitting `intentKind`** (`orderId`, `orderVersion`, order-line reference, wave), treating wave as sufficient to separate a compensating submit from the forward one
* **Three-part key omitting `orderVersion`** (`orderId`, order-line reference, wave), treating the line's identity as sufficiently stable across versions
* **Single shared identifier for both idempotency and correlation**, generated once per submit and reused as both the dedupe key and the trace identifier

## Decision Outcome

Chosen option: **five-part composite key**. The provisioning-intent idempotency key is:

    orderId + orderVersion + orderLineId + wave + intentKind

`intentKind` is the existing `owf_provisioning_intent.intent_kind` enum
(`draft_create | activation | draft_void | activated_cancel`). It is what makes a compensating key
structurally distinct from the forward key it undoes — draft-void carries a different key from the
draft-create it reverses, and activated-cancel from the activation it reverses — which the
four-component form cannot deliver, since a compensating intent shares the forward intent's order,
version, line and wave exactly.

For the wave-1 rebuild path a sixth component, `attempt` (integer, starting at 1), is appended. It
is the only component that changes across a rebuild, and it is persisted on
`owf_provisioning_intent.wave_attempt` so the key of any attempt is re-derivable rather than
remembered. A rebuild is therefore a new logical submit with a new key, not a resubmission under an
aged-out one.

Every key in every family is prefixed with `resource_tenant_id`, so keys are tenant-namespaced and
two tenants can never share a key space.

Two sibling key families exist alongside it, at their own granularity:

* **Approval requests**: `orderId + orderVersion + gateId`, with no line or wave component, since a
  gate is order-scoped. `gateId` is **derived deterministically** as a UUIDv5 over
  `(orderId, orderVersion, party)` rather than minted as a random uuid, so a crash between
  submitting to the Generic Approval service and committing the gate row re-derives the *same*
  identifier on replay, and therefore the same key and the same gate — a randomly minted `gateId`
  would produce a second gate with a second key and defeat approval idempotency outright.
* **Lifecycle transition calls**: `orderId + orderVersion + transitionName`, where `transitionName`
  is one of the five seam operations this gear may invoke on Orders Lifecycle. A transition call is
  order-scoped and has no line, wave or kind, and a retried reflection must be absorbed by
  Lifecycle rather than double-applied.

All key families are carried as explicit, independently addressable fields — never a pre-hashed
opaque value — so the reconciliation sweep can look up by component subsets
(`orderId` + `orderVersion` + line reference + wave) exactly as §6.3 requires, and so an operator
can read a stuck key and see which submit it names. No key in any family is ever reused as, or
derived from, the process `correlationId`.

### Consequences

* Every call site that constructs a provisioning intent must have `orderVersion` available at construction time, not just `orderId` and the line reference — this couples intent construction to the frozen per-version fulfillment plan (ADR-0004), which already carries `orderVersion` as part of its identity.
* Subscriptions must be able to reject a submit under a stale `orderVersion`'s key as a distinct in-flight identity from the current version's key, rather than treating the line reference alone as the dedupe scope — this is consistent with the upstream in-flight-rejection ask (`SUB-O11`) already required for retry handling.
* The reconciliation sweep and the manual-task/dead-letter surfaces must record and display every key component (or the corresponding correlationId-plus-components lookup tuple) so operators and the sweep can distinguish a superseded submit from a genuine duplicate, and a forward submit from the compensating submit that undoes it.
* Approval-request idempotency, being a narrower composite, cannot be satisfied by reusing the provisioning-intent key type; the design must define it as a distinct key shape from the outset rather than retrofitting it.
* Because the idempotency key is never permitted to double as `correlationId`, every intent payload carries two identifiers with different lifecycles: the correlationId may vary across a process's re-attempts of a step for tracing purposes, while the idempotency key must not vary across those same re-attempts for the same logical submit.
* `owf_provisioning_intent` must carry `intent_kind` and `wave_attempt` as key-bearing columns, and `UNIQUE (idempotency_key)` must be read as unique over the five (or six) components, not over the four; a forward intent and its compensating intent are two rows, not one row rewritten.
* `gateId` becomes a derived value, not a generated one: nothing may insert an approval gate with a randomly minted identifier, and the derivation inputs (`orderId`, `orderVersion`, `party`) must be resolved before the gate row is written.
* Every key-minting call site must have `resource_tenant_id` in hand, which makes tenant resolution a precondition of intent construction rather than a later enrichment step.

### Confirmation

Confirmed by a test that submits a wave-1 draft-create, retries it under a client-side timeout with the same idempotency key, and asserts Subscriptions returns the original outcome rather than creating a second subscription; by a test that amends the order to a new `orderVersion` before wave-1 completes and asserts the new version's draft-create intent carries a different idempotency key than the superseded version's, so it is not rejected as an in-flight duplicate; by a test asserting draft-create and activation for the same line and same order version carry different idempotency keys; by a test asserting a `draft_void` for a line carries a different idempotency key than the `draft_create` it reverses, and an `activated_cancel` a different key than the `activation` it reverses, so neither compensating submit is absorbed as a duplicate of its forward submit; by a test asserting a wave-1 rebuild increments `wave_attempt` and produces a key distinct from every prior attempt's; by a test asserting a `gateId` re-derived after a simulated crash equals the one derived before it; and by a static/schema check asserting no code path assigns the same value to both the idempotency-key field and the correlationId field on an outbound intent, and that no key is minted without a tenant prefix.

## Pros and Cons of the Options

### Five-part composite key (chosen)

* Good, because it is stable across retries of the same logical submit while remaining distinct across every dimension (version, line, wave, direction) that must not collide.
* Good, because keeping the key structurally separate from `correlationId` lets duplicate detection and process tracing evolve independently, matching the PRD's explicit prohibition on reusing one as the other.
* Good, because component-addressable fields (not an opaque hash) directly support the reconciliation sweep's documented lookup shape.
* Good, because a compensating submit is distinguishable from the forward submit it undoes by the key alone, both locally under `UNIQUE (idempotency_key)` and at Subscriptions.
* Neutral, because it requires five fields plus a tenant prefix to be threaded through every intent construction path rather than a single generated token.
* Bad, because it requires every call site to have `orderVersion` and `resource_tenant_id` correctly resolved at construction time — a bug that resolves the wrong version silently changes dedupe scope rather than failing loudly.

### Four-part key omitting `intentKind`

* Good, because it is one component shorter and was the form originally recorded here.
* Bad, because a compensating intent shares its forward intent's order, version, line and wave, so the two keys are byte-identical: the local uniqueness constraint rejects the compensating row, and Subscriptions returns the stored forward outcome, which the compensation path reads as success against a subscription that is still live. This is the stranded-active-subscription outcome ADR-0005 exists to prevent, reached through the key rather than through the saga.
* Bad, because it cannot express the wave-1 rebuild at all: the rebuild's five components are unchanged, so a deterministic composition re-derives a key that must not be resubmitted.

### Three-part key omitting `orderVersion`

* Good, because it is a simpler key with one fewer field to thread through intent construction.
* Bad, because it directly violates the PRD's explicit prohibition: a later version of the same line and wave would collide with a superseded submit's key, which could cause Subscriptions to reject the new submit as an in-flight duplicate of a submit that should already be moot, or to silently absorb it as a duplicate success.
* Bad, because it makes an amendment's re-provisioning of an already-in-flight line unsafe by construction, which the Lifecycle amendment path does not otherwise forbid.

### Single shared identifier for both idempotency and correlation

* Good, because it is the fewest identifiers to generate, carry, and reason about.
* Bad, because it directly violates the PRD's explicit prohibition on reusing the idempotency key as the correlationId.
* Bad, because a retry that must keep the idempotency key fixed would also freeze the correlation identity, making it impossible for tracing to distinguish separate process attempts of the same logical submit — the two identifiers exist to serve genuinely different, sometimes conflicting, stability requirements.

## More Information

This ADR assumes the two-wave structure of ADR-0004, whose wave component this key composition depends on, and is the identity/idempotency contract that ADR-0005's compensating actions (draft-void, activated-cancel) are dispatched under.

## Traceability

- **PRD**: [PRD.md](../PRD.md) — §6.3 Provisioning Intent to Subscriptions, §6.3 Retry with Bounded Attempts (caller-side duplicate protocol)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-workflow-fr-owf-provisioning-intent` — this decision is the exact key composition and the correlationId-distinctness rule that requirement mandates for every intent.
* `cpt-cf-bss-orders-workflow-fr-owf-retry` — this decision is what makes the caller-side duplicate-submission protocol's retry-with-same-key step safe and detectable.
* `cpt-cf-bss-orders-workflow-fr-owf-intent-sweep` — this decision's component-addressable key shape is what the reconciliation sweep's documented lookup (`orderId` + `orderVersion` + line reference + wave) depends on.
