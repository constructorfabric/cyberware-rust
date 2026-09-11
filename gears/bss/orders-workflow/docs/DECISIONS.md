<!-- CONFLUENCE_TITLE: [BSS]: Orders Workflow — Design Decisions Register -->
<!-- Related: ./design/, ./PRD.md, ./UPSTREAM_REQS.md | Owners: BSS Orders team -->

# Design Decisions — Orders Workflow

<!-- toc -->

- [How to use this document](#how-to-use-this-document)
- [A. Process engine (slice 01)](#a-process-engine-slice-01)
  - [D-01: Engine execution history is not the process-audit source of record](#d-01-engine-execution-history-is-not-the-process-audit-source-of-record)
  - [D-02: Four distinct bounds govern execution, not three](#d-02-four-distinct-bounds-govern-execution-not-three)
  - [D-03: The idempotency registry's outcomes are exhaustive, and a still-processing conflict is never inferred as success](#d-03-the-idempotency-registrys-outcomes-are-exhaustive-and-a-still-processing-conflict-is-never-inferred-as-success)
  - [D-04: The dead-letter record and the manual-task record are two separate objects; a failed step never grows a second inspectable object](#d-04-the-dead-letter-record-and-the-manual-task-record-are-two-separate-objects-a-failed-step-never-grows-a-second-inspectable-object)
- [B. Triggers and start (slice 02)](#b-triggers-and-start-slice-02)
  - [D-05: Trigger intake is a closed, exhaustive vocabulary, admission-checked before any instance spawns](#d-05-trigger-intake-is-a-closed-exhaustive-vocabulary-admission-checked-before-any-instance-spawns)
  - [D-06: A trigger observed for a superseded orderVersion is ignored, and terminating on supersession follows the same path as terminating on a terminal order event](#d-06-a-trigger-observed-for-a-superseded-orderversion-is-ignored-and-terminating-on-supersession-follows-the-same-path-as-terminating-on-a-terminal-order-event)
  - [D-07: Un-activated wave-1 drafts are voided on every termination path, including on supersession](#d-07-un-activated-wave-1-drafts-are-voided-on-every-termination-path-including-on-supersession)
  - [D-08: Correlation is three distinct identifiers, never conflated](#d-08-correlation-is-three-distinct-identifiers-never-conflated)
- [C. Approval execution (slice 03)](#c-approval-execution-slice-03)
  - [D-09: Approval idempotency keys are composed from orderId, orderVersion, and gateId](#d-09-approval-idempotency-keys-are-composed-from-orderid-orderversion-and-gateid)
  - [D-10: Escalation-timer ownership sits in this gear, undivided across the service boundary](#d-10-escalation-timer-ownership-sits-in-this-gear-undivided-across-the-service-boundary)
  - [D-11: Escalation-timer pause preserves accrued elapsed time on hold and on a Generic Approval outage, and never resets the window](#d-11-escalation-timer-pause-preserves-accrued-elapsed-time-on-hold-and-on-a-generic-approval-outage-and-never-resets-the-window)
  - [D-12: Verdict-source unavailability parks the process fail-closed in `submitted`, and the park never suspends the Lifecycle `submitted` TTL](#d-12-verdict-source-unavailability-parks-the-process-fail-closed-in-submitted-and-the-park-never-suspends-the-lifecycle-submitted-ttl)
  - [D-13: The verdict cache is keyed on orderId plus orderVersion, and a stale-version verdict is discarded rather than reflected](#d-13-the-verdict-cache-is-keyed-on-orderid-plus-orderversion-and-a-stale-version-verdict-is-discarded-rather-than-reflected)
  - [D-14: Every stored verdict names its deciding authority, and the phase-1 stand-in is recorded as that authority by name](#d-14-every-stored-verdict-names-its-deciding-authority-and-the-phase-1-stand-in-is-recorded-as-that-authority-by-name)
- [D. Fulfillment plan (slice 04)](#d-fulfillment-plan-slice-04)
  - [D-15: A pre-activation abort is structurally distinct from a line-execution failure](#d-15-a-pre-activation-abort-is-structurally-distinct-from-a-line-execution-failure)
  - [D-16: Dependency resolution is Catalog-owned, graph-validated, and frozen per orderId plus orderVersion before any provisioning intent; a bundle is one line item](#d-16-dependency-resolution-is-catalog-owned-graph-validated-and-frozen-per-orderid-plus-orderversion-before-any-provisioning-intent-a-bundle-is-one-line-item)
  - [D-17: Pending and failed payment authorization are two distinct, never-collapsed process outcomes](#d-17-pending-and-failed-payment-authorization-are-two-distinct-never-collapsed-process-outcomes)
  - [D-18: The downstream Subscriptions transition-request id is a join key on the fulfillment task, never mirrored into order state](#d-18-the-downstream-subscriptions-transition-request-id-is-a-join-key-on-the-fulfillment-task-never-mirrored-into-order-state)
- [E. Commercial policy — owned elsewhere, no code default](#e-commercial-policy--owned-elsewhere-no-code-default)
  - [D-19: Partial-failure policy overrides per product line or commercial tier are commercial policy, not a design default](#d-19-partial-failure-policy-overrides-per-product-line-or-commercial-tier-are-commercial-policy-not-a-design-default)
  - [D-20: Approver absence and delegation reassignment is commercial/process policy owned by Product, not resolved by this design](#d-20-approver-absence-and-delegation-reassignment-is-commercialprocess-policy-owned-by-product-not-resolved-by-this-design)
- [F. Provisioning intents (slice 05)](#f-provisioning-intents-slice-05)
  - [D-21: The idempotency key composes orderId, orderVersion, orderLineId, wave, and intentKind, and is structurally distinct from correlationId](#d-21-the-idempotency-key-composes-orderid-orderversion-orderlineid-wave-and-intentkind-and-is-structurally-distinct-from-correlationid)
  - [D-22: The binding reference is opaque and caller-owned; Subscriptions and OSS never interpret it as order state](#d-22-the-binding-reference-is-opaque-and-caller-owned-subscriptions-and-oss-never-interpret-it-as-order-state)
  - [D-23: A pre-activation draft re-read immediately before every activation intent is the sole detection mechanism for an auto-voided wave-1 draft](#d-23-a-pre-activation-draft-re-read-immediately-before-every-activation-intent-is-the-sole-detection-mechanism-for-an-auto-voided-wave-1-draft)
  - [D-24: A voided wave-1 draft is rebuilt by re-running wave 1 against the same frozen orderId and orderVersion, never by resuming into wave 2](#d-24-a-voided-wave-1-draft-is-rebuilt-by-re-running-wave-1-against-the-same-frozen-orderid-and-orderversion-never-by-resuming-into-wave-2)
  - [D-25: The reconciliation sweep becomes read-only for an intent once its idempotency-key lifetime elapses, and never resubmits under an aged-out key](#d-25-the-reconciliation-sweep-becomes-read-only-for-an-intent-once-its-idempotency-key-lifetime-elapses-and-never-resubmits-under-an-aged-out-key)
  - [D-26: The first activation intent, not wave-1 draft-create acceptance, is reported as the spawn signal to Orders Lifecycle](#d-26-the-first-activation-intent-not-wave-1-draft-create-acceptance-is-reported-as-the-spawn-signal-to-orders-lifecycle)
- [G. Saga and compensation (slice 06)](#g-saga-and-compensation-slice-06)
  - [D-27: Both waves are compensable with no intra-saga pivot; compensation runs in reverse order across two structurally distinct legs](#d-27-both-waves-are-compensable-with-no-intra-saga-pivot-compensation-runs-in-reverse-order-across-two-structurally-distinct-legs)
  - [D-28: Operational compensation is a reporting boundary to Lifecycle, never a Billing gate](#d-28-operational-compensation-is-a-reporting-boundary-to-lifecycle-never-a-billing-gate)
  - [D-29: Draft-void failure and activated-cancel failure are two structurally distinct manual-task reasons, never collapsed into one generic compensation-failure reason](#d-29-draft-void-failure-and-activated-cancel-failure-are-two-structurally-distinct-manual-task-reasons-never-collapsed-into-one-generic-compensation-failure-reason)
  - [D-30: A dedicated five-step cancellation-fencing sequence must complete in full before any compensated outcome is reported, and a late success still counts as a created subscription requiring compensation](#d-30-a-dedicated-five-step-cancellation-fencing-sequence-must-complete-in-full-before-any-compensated-outcome-is-reported-and-a-late-success-still-counts-as-a-created-subscription-requiring-compensation)
  - [D-31: The FulfillmentTask unilateral-cancel window closes at activation-intent acceptance, not at draft-create acceptance](#d-31-the-fulfillmenttask-unilateral-cancel-window-closes-at-activation-intent-acceptance-not-at-draft-create-acceptance)
- [H. Manual tasks, override, queue, overdue escalation (slice 07)](#h-manual-tasks-override-queue-overdue-escalation-slice-07)
  - [D-32: Exactly one actionable manual task is created per permanently failed line under the default remediation policy, from any of the four failed-entrance routes; fail-fast substitutes a non-actionable tracked incident instead](#d-32-exactly-one-actionable-manual-task-is-created-per-permanently-failed-line-under-the-default-remediation-policy-from-any-of-the-four-failed-entrance-routes-fail-fast-substitutes-a-non-actionable-tracked-incident-instead)
  - [D-33: An override is rejected outright whenever it cannot be verified against a matching, active Subscriptions record; there is no provisional-acceptance path](#d-33-an-override-is-rejected-outright-whenever-it-cannot-be-verified-against-a-matching-active-subscriptions-record-there-is-no-provisional-acceptance-path)
  - [D-34: The overdue-fulfillment escalation window never auto-terminals the order and never by itself marks any line failed](#d-34-the-overdue-fulfillment-escalation-window-never-auto-terminals-the-order-and-never-by-itself-marks-any-line-failed)
- [I. Hold, resilience, and workflow-mediated cancel (slice 08)](#i-hold-resilience-and-workflow-mediated-cancel-slice-08)
  - [D-35: A hold suspends this gear's own dispatch and timers only; it never pauses, reads, or owns the Subscriptions draft auto-void TTL, and a paused timer resumes with its remaining window, never the full window](#d-35-a-hold-suspends-this-gears-own-dispatch-and-timers-only-it-never-pauses-reads-or-owns-the-subscriptions-draft-auto-void-ttl-and-a-paused-timer-resumes-with-its-remaining-window-never-the-full-window)
  - [D-36: Dependency-retry resilience and the Generic Approval fail-closed park are structurally distinct mechanisms, never one code path branching on dependency name](#d-36-dependency-retry-resilience-and-the-generic-approval-fail-closed-park-are-structurally-distinct-mechanisms-never-one-code-path-branching-on-dependency-name)
- [J. Read projection and authorization (slice 09)](#j-read-projection-and-authorization-slice-09)
  - [D-37: Every system-actor grant requires a gateway-asserted service principal carrying a scope claim naming the calling gear; actor class alone is never sufficient](#d-37-every-system-actor-grant-requires-a-gateway-asserted-service-principal-carrying-a-scope-claim-naming-the-calling-gear-actor-class-alone-is-never-sufficient)
  - [D-38: Process-record retention defaults to at least 400 days at audit grade, configurable, independently of any durable-execution engine's own run-history retention, with the Orders Workflow platform-audit-policy owner as its named executor](#d-38-process-record-retention-defaults-to-at-least-400-days-at-audit-grade-configurable-independently-of-any-durable-execution-engines-own-run-history-retention-with-the-orders-workflow-platform-audit-policy-owner-as-its-named-executor)
- [K. Tuning-value working baselines (engine and intent path)](#k-tuning-value-working-baselines-engine-and-intent-path)
  - [D-39: The retry backoff curve is exponential with base 1 s, coefficient 2.0, a 30 s cap and full jitter, over a maximum of 5 submission attempts](#d-39-the-retry-backoff-curve-is-exponential-with-base-1-s-coefficient-20-a-30-s-cap-and-full-jitter-over-a-maximum-of-5-submission-attempts)
  - [D-40: The per-attempt timeout is 10 s, set from the downstream's service objective rather than from caller patience](#d-40-the-per-attempt-timeout-is-10-s-set-from-the-downstreams-service-objective-rather-than-from-caller-patience)
  - [D-41: The step deadline is 5 minutes per wave step, derived from the fulfillment service objective rather than chosen](#d-41-the-step-deadline-is-5-minutes-per-wave-step-derived-from-the-fulfillment-service-objective-rather-than-chosen)
  - [D-42: The nesting invariant over the four bounds is asserted at configuration load, and a violating configuration is refused at startup](#d-42-the-nesting-invariant-over-the-four-bounds-is-asserted-at-configuration-load-and-a-violating-configuration-is-refused-at-startup)
  - [D-43: A gear-wide retry budget caps retries at 10% of request volume with adaptive throttling, in addition to the per-request attempt cap](#d-43-a-gear-wide-retry-budget-caps-retries-at-10-of-request-volume-with-adaptive-throttling-in-addition-to-the-per-request-attempt-cap)
  - [D-44: Per-order line parallelism is 8; the cross-process aggregate limit is sized by Little's Law from measured capacity and preferably adaptive; the dead-letter delivery cap is 5](#d-44-per-order-line-parallelism-is-8-the-cross-process-aggregate-limit-is-sized-by-littles-law-from-measured-capacity-and-preferably-adaptive-the-dead-letter-delivery-cap-is-5)
  - [D-45: The reconciliation-sweep ladder is 30 s to 1 h with jitter and a bounded page size, and must resolve every intent inside the overdue window](#d-45-the-reconciliation-sweep-ladder-is-30-s-to-1-h-with-jitter-and-a-bounded-page-size-and-must-resolve-every-intent-inside-the-overdue-window)
  - [D-46: Generic Approval outage detection is fixed with a circuit breaker, while the escalation threshold remains deliberately unset](#d-46-generic-approval-outage-detection-is-fixed-with-a-circuit-breaker-while-the-escalation-threshold-remains-deliberately-unset)
- [L. Cross-cutting decisions (remediation)](#l-cross-cutting-decisions-remediation)
  - [D-47: Idempotency keys form four tenant-prefixed families, and `gateId` is derived rather than minted](#d-47-idempotency-keys-form-four-tenant-prefixed-families-and-gateid-is-derived-rather-than-minted)
  - [D-48: Three tenant axes, and every gear-owned table carries at least the resource axis](#d-48-three-tenant-axes-and-every-gear-owned-table-carries-at-least-the-resource-axis)
  - [D-49: Optimistic concurrency is a row version surfaced as an ETag, and one active instance per order is a partial unique index](#d-49-optimistic-concurrency-is-a-row-version-surfaced-as-an-etag-and-one-active-instance-per-order-is-a-partial-unique-index)
  - [D-50: The audit log is append-only and hash-chained, and human justification is a separate column from the reason code](#d-50-the-audit-log-is-append-only-and-hash-chained-and-human-justification-is-a-separate-column-from-the-reason-code)
  - [D-51: `parked` is a distinct process-phase value](#d-51-parked-is-a-distinct-process-phase-value)
  - [D-52: The reconciliation-sweep ladder splits by phase](#d-52-the-reconciliation-sweep-ladder-splits-by-phase)
  - [D-53: A non-pausable `max_process_lifetime` of 90 days bounds every process, independently of order state](#d-53-a-non-pausable-max_process_lifetime-of-90-days-bounds-every-process-independently-of-order-state)
  - [D-54: A partial wave-2 failure holds the order; already-activated lines are not rolled back](#d-54-a-partial-wave-2-failure-holds-the-order-already-activated-lines-are-not-rolled-back)
  - [D-55: "Remediation exhausted" is three failed attempts on the same task, or the manual-task SLA deadline elapsing](#d-55-remediation-exhausted-is-three-failed-attempts-on-the-same-task-or-the-manual-task-sla-deadline-elapsing)
  - [D-56: The submitting identity is refused at the approval-decision endpoint](#d-56-the-submitting-identity-is-refused-at-the-approval-decision-endpoint)
  - [D-57: `lifecycle_submitted_ttl` is mirrored as local configuration under a startup refusal, pending the upstream field](#d-57-lifecycle_submitted_ttl-is-mirrored-as-local-configuration-under-a-startup-refusal-pending-the-upstream-field)
- [Open Questions](#open-questions)
  - [Q-01: Which durable-execution substrate backs the process — the OSS Workflow Engine or a BSS-local mechanism?](#q-01-which-durable-execution-substrate-backs-the-process--the-oss-workflow-engine-or-a-bss-local-mechanism)
  - [Q-02: The Generic Approval escalation threshold — the one PRD-deferred numeric value this design deliberately leaves unset](#q-02-the-generic-approval-escalation-threshold--the-one-prd-deferred-numeric-value-this-design-deliberately-leaves-unset)
  - [Q-03: Does a bounded hold/resume cycle count or bound the total elapsed non-terminal lifetime of an order, beyond the single-cycle remaining-window guarantee this design already keeps?](#q-03-does-a-bounded-holdresume-cycle-count-or-bound-the-total-elapsed-non-terminal-lifetime-of-an-order-beyond-the-single-cycle-remaining-window-guarantee-this-design-already-keeps)
  - [Q-04: The SUB-O renumbering and the missing OrderAmended re-verdict requirement — two required PRD amendments](#q-04-the-sub-o-renumbering-and-the-missing-orderamended-re-verdict-requirement--two-required-prd-amendments)
  - [Q-05: The Generic Approval service has no specification anywhere in this repository, leaving the whole approval capability inert in phase 1](#q-05-the-generic-approval-service-has-no-specification-anywhere-in-this-repository-leaving-the-whole-approval-capability-inert-in-phase-1)
  - [Q-06: Payments has no specification and no register, so the payment-authorization ask has no owner; only "provision first, collect after" is expressible](#q-06-payments-has-no-specification-and-no-register-so-the-payment-authorization-ask-has-no-owner-only-provision-first-collect-after-is-expressible)
  - [Q-07: Tension between asynchronous outbox publication and the PRD's p95 < 30 s process-event delivery target](#q-07-tension-between-asynchronous-outbox-publication-and-the-prds-p95--30-s-process-event-delivery-target)
  - [Q-08: SUB-O5 (overlap-scope-key presence read) is unagreed, leaving the pre-activation overlap check unevaluable; SUB-O1 (compensation cancellation reason) is critical and unagreed, leaving the activated-cancel reason unspecifiable from this side](#q-08-sub-o5-overlap-scope-key-presence-read-is-unagreed-leaving-the-pre-activation-overlap-check-unevaluable-sub-o1-compensation-cancellation-reason-is-critical-and-unagreed-leaving-the-activated-cancel-reason-unspecifiable-from-this-side)
- [What This Design Set Does Not Claim](#what-this-design-set-does-not-claim)
- [Traceability](#traceability)

<!-- /toc -->

## How to use this document

Every call taken while authoring the design set lands here. An entry records the **decision**
(stated as a resolved fact, never as a debate or an option list), its **rationale** (including
what was rejected and why), and its **propagation** address — the document and section that must
agree with it. A decision this design may take is marked as such; a decision that is commercial
policy owned elsewhere is marked with no code default, so an unset value is visible as absent
behaviour rather than silently becoming the platform answer.

This is the whole register for the gear, in one file. It covers every slice — the process engine
(01), triggers and start (02), approval execution (03), the fulfillment plan (04), provisioning
intents (05), saga and compensation (06), manual tasks (07), hold and cancel (08), and read and
authorization (09) — together with the commercial-policy decisions owned elsewhere, the
tuning-value working baselines, the cross-cutting decisions, and the open-questions register.
Numbering starts at `D-01`, runs in one continuous sequence, and is never split across parts.
Reopening a decision means flipping its status and recording why; no existing identifier is ever
renumbered.

Where a decision restates one already recorded as an ADR, the entry names that ADR. The two
registers are one system of record read from two angles: the ADR carries the options considered
and the reasoning; the entry here carries the resolved fact and its propagation address. Where they
would disagree, the ADR governs the rationale and this register governs the propagation.

**Note on validation coverage**: `DECISIONS.md` is not a registered `cfs` artifact kind for this
gear (`docs/DECISIONS.md` is excluded from `cfs` autodetect per `.cf-studio/config/artifacts.toml`), so
`cfs validate --artifact` reports it unmatched by design — this is expected, not a defect. This
document relies on `cfs toc`, `cfs validate-toc`, and `cfs check-language` instead.

## A. Process engine (slice 01)

### D-01: Engine execution history is not the process-audit source of record

**Decision**: `owf_step_log` (and any durable-execution substrate run history) exists solely for
recovery and replay. `owf_audit_entry` is the sole audit source of record, append-only, with no
UPDATE or DELETE path. A durable-execution substrate is used for retries and durable timers, but
its own run history is never queried as, or substituted for, the process audit trail.

**Rationale**: substrate-side history is purge- and retention-policy-controlled by the substrate,
not by this gear, and the PRD's audit-retention NFR must survive independently of any engine
purge; treating substrate history as authoritative would make gear-owned audit retention hostage
to an infrastructure choice made via a separate ADR.

**ADR**: ADR-0001 (`cpt-cf-bss-orders-workflow-adr-durable-execution-substrate`) — the audit-source-of-record decision this entry restates. ADR-0001 selects no engine; the substrate choice is Q-01 below.

**Propagates to**: `gears/bss/orders-workflow/docs/design/01-foundation.md` § `4.1 Engine
execution history is not the audit source of record`

### D-02: Four distinct bounds govern execution, not three

**Decision**: retry budget (governs submission failures only), per-attempt timeout, and step
deadline are tracked together in `owf_retry_state`. The process deadline (the overdue window) is
a fourth, structurally separate bound, tracked as a `timer_kind` value in `owf_durable_timer`. The
process deadline MUST NOT by itself mark any line failed or auto-terminal the order.

**Rationale**: keeping the process deadline in a different table from the retry-budget/per-attempt
timeout/step-deadline trio makes the distinction structural rather than a column a handler could
accidentally conflate with a punitive bound; the PRD-driven overdue window exists to trigger
escalation, not to punish an order that is merely running long.

**Propagates to**: `gears/bss/orders-workflow/docs/design/01-foundation.md` § `4.2 Four distinct
bounds, not one`

### D-03: The idempotency registry's outcomes are exhaustive, and a still-processing conflict is never inferred as success

**Decision**: every idempotency-key lookup resolves to exactly one of: first call, absorbed
duplicate (settled, matching fingerprint, closure not re-invoked), still-processing conflict, or
aged-out (retention window elapsed, sweep becomes read-only). No fifth outcome exists. A handler
receiving a still-processing conflict MUST NOT advance any process-owned field, MUST NOT infer
success, and MUST NOT resubmit under a new key.

**Rationale**: an exhaustive, closed outcome set is what lets every handler slice use the same
caller-side duplicate protocol without inventing its own interpretation of an ambiguous
in-flight state; inferring success from silence or from a conflict is precisely the failure mode
that produces double-provisioning.

**Propagates to**: `gears/bss/orders-workflow/docs/design/01-foundation.md` § `4.3 The
idempotency registry's non-success outcomes are exhaustive`

### D-04: The dead-letter record and the manual-task record are two separate objects; a failed step never grows a second inspectable object

**Decision**: `owf_dead_letter_record` records **delivery** exhaustion, never step exhaustion. It
is written only when an inbound Lifecycle trigger, or a Subscriptions / Payments /
Generic-Approval callback, exhausts its finite delivery-count cap; it is written by the step
executor in its delivery-intake role, and it is never an order state. A fulfillment step that
exhausts its retry budget does **not** reach this store by construction — its inspectable object
is the manual task (remediation policy) or the tracked incident (fail-fast), and that object is
the only one it gets. A failure therefore has exactly one inspectable object, but which store
holds it is decided by the failure's class — delivery-level or step-level — not by which code
path noticed first.

**Rationale**: giving one failure two inspectable objects would let an operator resolve one and
leave the other stale, and would double-count the same failure in two different operator queues.
The earlier formulation of this entry — calling the dead-letter record "the step executor's own
record of retry-budget exhaustion" — collapsed exactly the distinction it was written to preserve
and contradicted ADR-0009, which scopes the store to delivery exhaustion only. ADR-0009 governs;
this entry is corrected to it. The residue to watch for is `owf_step_log.outcome` still carrying
`dead-lettered`: that value denotes a delivery-level outcome observed by a step, never a step
outcome, and it does not license a dead-letter row for a failed step.

**ADR**: ADR-0009 (`cpt-cf-bss-orders-workflow-adr-manual-task-dead-letter-separation`) — governs this entry; the dead-letter store is delivery-scoped, never step-scoped.

**Propagates to**: `gears/bss/orders-workflow/docs/design/01-foundation.md` § `4.8 The
dead-letter record is never an order state`

## B. Triggers and start (slice 02)

### D-05: Trigger intake is a closed, exhaustive vocabulary, admission-checked before any instance spawns

**Decision**: Trigger Intake subscribes to the Orders Lifecycle state-event stream (event
subscription, not a command surface Lifecycle calls into) and recognizes a closed trigger
vocabulary. Exactly one active process instance per order id is enforced at admission — Trigger
Intake checks for an existing active instance before spawning a new one, not by later
reconciliation.

**Rationale**: Lifecycle already publishes these events to other consumers, so a command push
would invert the publisher/consumer ownership Lifecycle already holds; event subscription also
gives at-least-once-plus-dedup-by-event-id delivery for free, matching the duplicate-absorption
requirement. Enforcing the one-active-instance rule at admission (rather than by reconciling
after the fact) avoids a window in which two instances could race on the same order.

**Propagates to**: `gears/bss/orders-workflow/docs/design/02-triggers-and-start.md` § `2.1 The
trigger vocabulary is closed and exhaustive` and § `2.1 Exactly one active instance per order id`

### D-06: A trigger observed for a superseded orderVersion is ignored, and terminating on supersession follows the same path as terminating on a terminal order event

**Decision**: an `OrderAmended` trigger observed while a process instance is still active for the
pre-amendment version routes through the same Termination and Compensation component as a
terminal order event: cancel open approvals and escalation timers, cease pending provisioning
intents, run compensation, void un-activated wave-1 drafts, and record termination in the process
audit log. A trigger that names an orderVersion this gear has already superseded is ignored, not
acted on.

**Rationale**: treating "superseded by amendment" as a second, different termination path from
"terminal event" would create a drift channel where the two paths could silently diverge; folding
them into one component removes that channel by construction.

**Propagates to**: `gears/bss/orders-workflow/docs/design/02-triggers-and-start.md` § `3.6
Terminal-event compensation, void, and audit`

### D-07: Un-activated wave-1 drafts are voided on every termination path, including on supersession

**Decision**: voiding un-activated wave-1 subscription drafts is a mandatory step of the shared
Termination and Compensation sequence (D-06), executed identically whether termination is caused
by a terminal Lifecycle event or by supersession via `OrderAmended`.

**Rationale**: a wave-1 draft left un-voided after the order it belongs to is superseded or
terminated would be an orphaned subscription artifact with no order to reconcile against.

**Propagates to**: `gears/bss/orders-workflow/docs/design/02-triggers-and-start.md` § `3.6
Terminal-event compensation, void, and audit`

### D-08: Correlation is three distinct identifiers, never conflated

**Decision**: the process `correlationId` (generated once at process start, whole-instance
scope), the per-call idempotency key (one call plus its retries), and the downstream
`TransitionRequest` id (one Subscriptions request) are three distinct identifiers. No component
derives one from another, and no idempotency key anywhere in this gear reuses `correlationId`.

**Rationale**: a single process instance can open multiple concurrent approval gates and
re-open gates across order versions, so an idempotency key derived from the whole-instance
`correlationId` would either collide across gates or fail to distinguish versions.

**Propagates to**: `gears/bss/orders-workflow/docs/design/02-triggers-and-start.md` § `2.1
Correlation is layered, not collapsed`

## C. Approval execution (slice 03)

### D-09: Approval idempotency keys are composed from orderId, orderVersion, and gateId

**Decision**: every approval-gate idempotency key is `orderId` + `orderVersion` + `gateId`. It
never reuses the process `correlationId` (per D-08).

**Rationale**: a `correlationId`-derived key would collide across a multi-party instance's
concurrent gates and would fail to distinguish a re-opened gate on an amended version from the
gate it superseded.

**ADR**: ADR-0006 (`cpt-cf-bss-orders-workflow-adr-idempotency-key-composition`) — the approval-request key family, including the deterministic `gateId` derivation.

**Propagates to**: `gears/bss/orders-workflow/docs/design/03-approval-execution.md` § `2.2
Idempotency keys must not reuse the process correlation id`

### D-10: Escalation-timer ownership sits in this gear, undivided across the service boundary

**Decision**: the Escalation Timer Owner component is the sole scheduler, persister, and firer of
every approval gate's escalation timer. The Generic Approval service supplies configuration only
(window, escalation path) and never stores timer state.

**Rationale**: splitting timer ownership across the service boundary is the exact failure mode
that leaves neither side certain which one owns the clock during a partial outage; keeping
ownership undivided in this gear removes that ambiguity.

**Propagates to**: `gears/bss/orders-workflow/docs/design/03-approval-execution.md` § `2.1 Timer
ownership never splits across a service boundary`

### D-11: Escalation-timer pause preserves accrued elapsed time on hold and on a Generic Approval outage, and never resets the window

**Decision**: hold and a Generic Approval service outage on an already-open gate use the same
pause mechanism owned by the Escalation Timer Owner (D-10). On resume, the timer's previously
accrued elapsed time is preserved rather than restarted from zero.

**Rationale**: the PRD forbids the escalation window burning into a dead dependency; resetting to
zero on resume would let repeated short outages extend the effective SLA indefinitely, which is
the opposite of the escalation window's purpose.

**ADR**: ADR-0007 (`cpt-cf-bss-orders-workflow-adr-fail-closed-verdict-park`) — the gate-pause-on-outage consequence this entry restates.

**Propagates to**: `gears/bss/orders-workflow/docs/design/03-approval-execution.md` § `3.6
Escalation timer fire and approval-service outage pause`

### D-12: Verdict-source unavailability parks the process fail-closed in `submitted`, and the park never suspends the Lifecycle `submitted` TTL

**Decision**: every verdict-source or approval-service unavailability path parks or pauses the
process rather than assuming success; the order remains in Lifecycle's `submitted` state. This
gear never fails open to `approved`. The park does not suspend the Lifecycle `submitted` TTL, so
this gear escalates to the operator queue before that TTL elapses.

**Rationale**: fail-open on an unevaluable approval verdict would let an order proceed to
fulfillment with no policy check ever having run; the alternative of suspending the TTL was
rejected because Lifecycle's TTL is a Lifecycle-owned bound this gear cannot silently extend.

**ADR**: ADR-0007 (`cpt-cf-bss-orders-workflow-adr-fail-closed-verdict-park`) — the fail-closed park decision this entry restates, including the `parked` process phase.

**Propagates to**: `gears/bss/orders-workflow/docs/design/03-approval-execution.md` § `2.1
Fail-closed, never fail-open, never auto-reject`

### D-13: The verdict cache is keyed on orderId plus orderVersion, and a stale-version verdict is discarded rather than reflected

**Decision**: the approval-requirement verdict is cached per `orderId` + `orderVersion`. On
`OrderAmended`, the Verdict Gateway re-queries the verdict for the new version from scratch — it
never derives the new version's verdict from the version it superseded. A verdict result that
arrives for a version this gear has already moved past is discarded as stale, never reflected
onward.

**Rationale**: mirrors the sibling Orders Lifecycle design's normative rule that deriving an
amended version's verdict from the version it superseded is forbidden; a stale verdict reflected
onward would apply a policy decision made against commercial facts the order no longer has.

**Propagates to**: `gears/bss/orders-workflow/docs/design/03-approval-execution.md` § `3.2
Verdict Gateway` and § `3.6 Verdict retrieval and reflection on OrderSubmitted`

### D-14: Every stored verdict names its deciding authority, and the phase-1 stand-in is recorded as that authority by name

**Decision**: until the Generic Approval service exists, every verdict query resolves through a
named stand-in acting for `cpt-cf-bss-orders-workflow-actor-owf-generic-approval`, behind the
PRD §9.2 expectations contract — not a second policy author. It always returns "approval not required,"
and every such verdict is recorded with the stand-in named as the deciding authority. As a
consequence, multi-party gates, escalation timers, the approver inbox, and the
`OrderApprovalRequested` / `OrderApprovalEscalated` events never fire in phase 1 — the approval
capability is inert until the Generic Approval service exists.

**Rationale**: naming the stand-in as the deciding authority on every stored verdict is the only
field that lets a later audit distinguish an order the policy actually exempted from an order
nobody ever asked about, given the stand-in currently exempts everything.

**Propagates to**: `gears/bss/orders-workflow/docs/design/03-approval-execution.md` § `4
Disclosure 1 — the whole capability is inert in phase 1`

## D. Fulfillment plan (slice 04)

### D-15: A pre-activation abort is structurally distinct from a line-execution failure

**Decision**: an overlap collision or market divergence detected immediately before the first
activation intent voids wave-1 drafts and acknowledges `fulfillment_failed` directly. It never
touches `FulfillmentTask.state` and never enters the remediate/fail-fast partial-failure policy
that governs a real line-execution failure.

**Rationale**: by construction nothing beyond a `draft` subscription has ever existed when a
pre-activation abort fires, so there is nothing to remediate and no partial outcome to reconcile;
conflating the two paths would let an operator retry a condition with no correctable root cause,
or let a real provisioning failure escape compensation.

**Propagates to**: `gears/bss/orders-workflow/docs/design/04-fulfillment-plan.md` § `2.2
Pre-Activation Abort Is Not a Line Failure` and § `3.6 Pre-Activation Abort`

### D-16: Dependency resolution is Catalog-owned, graph-validated, and frozen per orderId plus orderVersion before any provisioning intent; a bundle is one line item

**Decision**: the order document carries no dependency data. The Plan Constructor reads Catalog's
published topology, builds exactly one `FulfillmentTask` per order line item — bundle lines are
never expanded into multiple tasks — validates the resulting graph acyclic with every dependency
present among the order's own lines, and hands the result to the Plan Freeze Store, which is
keyed on `orderId` + `orderVersion` and rejects any post-freeze mutation. An invalid graph halts
fulfillment before any subscription is created and needs no compensation.

**Rationale**: this gear does not own product topology, so re-deriving or caching dependency data
locally would create a second, driftable copy of Catalog's own data; freezing the plan per
version before any provisioning intent prevents a mid-execution amendment from mutating a plan
that provisioning has already started acting on.

**Propagates to**: `gears/bss/orders-workflow/docs/design/04-fulfillment-plan.md` § `2.1
Dependency Ownership Stays With Catalog`, § `2.2 Bundle Lines Are Never Expanded`, and § `3.6 Plan
Construction and Freeze`

### D-17: Pending and failed payment authorization are two distinct, never-collapsed process outcomes

**Decision**: payment-authorization pending and payment-authorization failed are kept as two
distinct begin-fulfillment guard outcomes, never collapsed into one. Tolerate-failure and
recorded-buyer-acceptance are separate begin-fulfillment guards, and `OrderAcceptanceRecorded`
re-triggers begin-fulfillment eligibility evaluation. Payments' mechanism is consumed as an
opaque outcome only.

**Rationale**: collapsing "pending" (an asynchronous wait) into "failed" (a terminal negative
result) would force an order awaiting an SCA-style challenge down the same path as an order whose
charge was declined, which are not the same commercial situation and do not warrant the same
process response.

**Propagates to**: `gears/bss/orders-workflow/docs/design/04-fulfillment-plan.md` § `2.1 Pending
and Failed Are Distinct Outcomes`

### D-18: The downstream Subscriptions transition-request id is a join key on the fulfillment task, never mirrored into order state

**Decision**: the transition-request identifier returned by Subscriptions for a provisioning
intent is recorded on the corresponding `FulfillmentTask` row as a join key only. It is never
copied onto the order document or any Lifecycle-owned field.

**Rationale**: keeps process-owned correlation data separate from the commercial order aggregate
that Lifecycle owns, consistent with the dual-authority principle (this gear drives process,
Lifecycle owns commercial state).

**Propagates to**: `gears/bss/orders-workflow/docs/design/04-fulfillment-plan.md` § `3.7 Table:
owf_fulfillment_task`

## E. Commercial policy — owned elsewhere, no code default

### D-19: Partial-failure policy overrides per product line or commercial tier are commercial policy, not a design default

**Decision**: whether the default partial-failure policy ("continue independent lines, halt
dependents") may be overridden per product line or commercial tier is commercial policy owned by
Product, not a call this design takes. This design implements only the default and provides no
per-tier override mechanism until Product resolves the question.

**Rationale**: defaulting an override behaviour in code before Product has decided whether one is
needed would make an unset commercial choice silently become the platform answer; leaving it
absent keeps that gap visible.

**Propagates to**: `gears/bss/orders-workflow/docs/PRD.md` § `15. Open Questions` (partial-failure
policy defaults per product line)

### D-20: Approver absence and delegation reassignment is commercial/process policy owned by Product, not resolved by this design

**Decision**: what happens when an approval gate's principal has left, changed role, or lost
scope is Product-owned policy. This design's only implemented behaviour for an open, unresolved
gate is escalation per D-10/D-11; reassignment of an open gate is out of scope and has no code
default.

**Rationale**: inventing a reassignment mechanism without a Product-confirmed rule would encode a
guess as platform behaviour; escalation is the one behaviour the PRD already authorizes as an
interim answer.

**Propagates to**: `gears/bss/orders-workflow/docs/PRD.md` § `15. Open Questions` (approver
absence and delegation)

## F. Provisioning intents (slice 05)

### D-21: The idempotency key composes orderId, orderVersion, orderLineId, wave, and intentKind, and is structurally distinct from correlationId

**Decision**: the idempotency key for every provisioning intent is five components —
`orderId` + `orderVersion` + `orderLineId` + `wave` + `intentKind` — tenant-prefixed with
`resource_tenant_id`. `intentKind` is the existing `owf_provisioning_intent.intent_kind` enum
(`draft_create | activation | draft_void | activated_cancel`). `orderVersion` is mandatory. The
key is never reused as `correlationId`, and `correlationId` is never substituted for it. For the
wave-1 rebuild path a sixth component, `attempt` (integer from 1, persisted on
`owf_provisioning_intent.wave_attempt`), is appended; it is the only component a rebuild changes.

**Rationale**: omitting `orderVersion` was rejected because a superseding amendment would then
collide with the prior version's key and either mask a legitimate resubmission as a duplicate or
let a stale-version retry pass as current. `intentKind` is required for a sharper reason: a
compensating intent shares its forward intent's order, version, line **and wave**, so a
four-component key makes `draft_void` byte-identical to the `draft_create` it reverses and
`activated_cancel` identical to the `activation` it reverses. `UNIQUE (idempotency_key)` then
rejects the compensating row locally, and Subscriptions returns the stored forward outcome as an
absorbed duplicate — so compensation records success against a subscription that is still live.
The key's purpose (submission de-duplication) and the correlationId's purpose (whole-process
tracing) diverge in lifetime and semantics, so conflating them would let an aged-out key be reused
to resubmit or lose traceability once the key expires. The `attempt` component exists because a
deterministic composition otherwise re-derives an identical key for the rebuild, which the design
forbids resubmitting.

**ADR**: ADR-0006 (`cpt-cf-bss-orders-workflow-adr-idempotency-key-composition`) — the five-component key composition this entry restates.

**Propagates to**: `gears/bss/orders-workflow/docs/design/05-provisioning-intents.md` § `2.2 The
Idempotency Key Is Not the Correlation Identifier`

### D-22: The binding reference is opaque and caller-owned; Subscriptions and OSS never interpret it as order state

**Decision**: every provisioning intent additionally carries an opaque, caller-owned binding
reference (`binding_reference`), which may be derived from the order's external reference where
one exists. Subscriptions and OSS consume it as an opaque token only and never interpret it as
order state.

**Rationale**: giving Subscriptions or OSS a field they could read as order state would create a
second, driftable channel for commercial order content outside Lifecycle's system of record;
keeping the reference opaque preserves the dual-authority boundary this gear's process state
already observes toward order state.

**Propagates to**: `gears/bss/orders-workflow/docs/design/05-provisioning-intents.md` §
`3.7 Database schemas & tables` (`owf_provisioning_intent`, `binding_reference`)

### D-23: A pre-activation draft re-read immediately before every activation intent is the sole detection mechanism for an auto-voided wave-1 draft

**Decision**: the Draft-Liveness Re-reader re-reads the wave-1 draft's liveness immediately before
every activation intent, and this re-read is the only detection mechanism this design relies on
for an auto-voided draft, regardless of cause (hold, platform TTL, or otherwise). No design path
depends on Subscriptions emitting a draft-void notification.

**Rationale**: Subscriptions is not obligated to emit a void notification for every voiding cause,
so a design depending on one would silently miss cases; re-reading immediately before the intent
that depends on liveness is the one point where staleness is guaranteed to be caught before it can
matter.

**Propagates to**: `gears/bss/orders-workflow/docs/design/05-provisioning-intents.md` §
`2.1 Detection by Re-read, Never by Absence of Notification`

### D-24: A voided wave-1 draft is rebuilt by re-running wave 1 against the same frozen orderId and orderVersion, never by resuming into wave 2

**Decision**: when the pre-activation re-read finds a voided draft, the Wave-1 Rebuilder issues a
fresh draft-create intent with a fresh idempotency key, against the same frozen `orderId` and
`orderVersion` already established for that line; it never attempts to proceed directly into wave
2 against the voided draft.

**Rationale**: an activation intent has no meaning without a live draft to activate, and rebuilding
against the same frozen version (rather than re-resolving the plan) keeps the rebuild consistent
with the plan-freeze decision already taken for dependency resolution.

**ADR**: ADR-0006 (`cpt-cf-bss-orders-workflow-adr-idempotency-key-composition`) — the rebuild `attempt` component is what makes the rebuild key fresh.

**Propagates to**: `gears/bss/orders-workflow/docs/design/05-provisioning-intents.md` §
`3.2 Component Model` (Wave-1 Rebuilder)

### D-25: The reconciliation sweep becomes read-only for an intent once its idempotency-key lifetime elapses, and never resubmits under an aged-out key

**Decision**: past the idempotency-key lifetime, the reconciliation sweep switches its lookup
strategy to `correlationId` plus `orderId`/`orderVersion`/line/wave, or the recorded
transition-request identifier (`SUB-O13`), and only re-reads status from that point forward. It
never invents a new idempotency key on a retry and never resubmits a submission under the aged-out
key.

**Rationale**: reusing or re-deriving a key past its lifetime would risk a second acceptance for a
submission Subscriptions may already have progressed past de-duplication for; read-only recovery
by correlation identifiers keeps reconciliation safe without depending on submission
de-duplication that is no longer guaranteed to apply.

**Propagates to**: `gears/bss/orders-workflow/docs/design/05-provisioning-intents.md` §
`3.2 Component Model` (Reconciliation Sweep)

### D-26: The first activation intent, not wave-1 draft-create acceptance, is reported as the spawn signal to Orders Lifecycle

**Decision**: this gear reports the spawn signal to Orders Lifecycle at the first activation
intent's confirmation, never at wave-1 draft-create acceptance.

**Rationale**: a `draft` subscription is not resource-affecting and does not commit the order to
provisioning; reporting spawn on draft-create acceptance would cause Lifecycle to treat the order
as spawned prematurely, before any activation has actually begun.

**Propagates to**: `gears/bss/orders-workflow/docs/design/05-provisioning-intents.md` §
`3.6 Interactions & Sequences` (Two-Wave Provisioning Dispatch)

## G. Saga and compensation (slice 06)

### D-27: Both waves are compensable with no intra-saga pivot; compensation runs in reverse order across two structurally distinct legs

**Decision**: wave-1 create is compensated by draft-void and wave-2 activation by
activated-cancel; there is no irreversible branch and no intra-saga pivot anywhere in this
design. Activation changes which leg applies, never whether a compensating leg exists.
Compensation for a multi-line order runs in reverse order of forward execution. OSS-side
irreversibility is absorbed inside Subscriptions' own cancel handling — this gear never reaches
past Subscriptions to undo OSS directly, in compensation any more than in forward execution.

**Rationale**: an irreversible branch would leave some failure combinations with no rollback path,
directly producing the stranded-subscription risk this design's own risk register names; keeping
both legs symmetric and running them in reverse order avoids compensating a line whose sibling
dependency has not yet been unwound.

**ADR**: ADR-0005 (`cpt-cf-bss-orders-workflow-adr-saga-compensable-no-pivot`) — the compensable/no-pivot classification and the wave-grouped unwind order this entry restates.

**Propagates to**: `gears/bss/orders-workflow/docs/design/06-saga-and-compensation.md` §
`2.1 No Intra-Saga Pivot`

### D-28: Operational compensation is a reporting boundary to Lifecycle, never a Billing gate

**Decision**: operational compensation (draft-void, activated-cancel) completes or escalates on
its own timeline and is reported to Lifecycle the moment it reaches a known outcome. Reversal of
posted at-sale billable facts is tracked separately in the billing chain and is never awaited;
`CompensationRecord` records whether at-sale facts had been emitted as evidence only, never as a
gate.

**Rationale**: waiting on billing reversal before reporting an operational outcome would tie an
operational timeline to a financial-reconciliation timeline with different SLAs and a different
system of record, which this gear does not own.

**Propagates to**: `gears/bss/orders-workflow/docs/design/06-saga-and-compensation.md` §
`2.2 Operational Compensation Never Waits on Billing`

### D-29: Draft-void failure and activated-cancel failure are two structurally distinct manual-task reasons, never collapsed into one generic compensation-failure reason

**Decision**: `CompensationFailureReason` enumerates exactly `draft-void-failed` and
`activated-cancel-failed`. A compensating action that cannot complete never invents a third
compensating action; it escalates via the existing manual-task path with its own leg-scoped
reason and leaves the order non-terminal.

**Rationale**: collapsing the two into one generic "compensation failed" reason would erase, at
the operator queue, which leg (a still-draft subscription versus an active one) actually needs
remediation, which changes the operator's remediation action.

**ADR**: ADR-0005 (`cpt-cf-bss-orders-workflow-adr-saga-compensable-no-pivot`) — the leg-specific manual-task reasons this entry restates.

**Propagates to**: `gears/bss/orders-workflow/docs/design/06-saga-and-compensation.md` §
`2.2 Distinct Manual-Task Reasons per Leg`

### D-30: A dedicated five-step cancellation-fencing sequence must complete in full before any compensated outcome is reported, and a late success still counts as a created subscription requiring compensation

**Decision**: the Cancellation-Fencing Component runs a strict, ordered five-step sequence — (1)
stop dispatching new provisioning intents, (2) identify all intents already accepted by or in
flight to Subscriptions, (3) wait for and reconcile terminal outcomes via the existing
reconciliation sweep, (4) compensate every created subscription including late successes, (5)
verify no active subscription remains — and all five steps must complete before any compensated
outcome is reported. A confirmation that arrives after cancellation was requested is still treated
as a created subscription and is compensated, never discarded as moot. Superseding an accepted
in-flight intent uses `SUB-O12` (cancel/void), never a second submit.

**Rationale**: reporting a compensated outcome before every in-flight intent has reached a terminal
outcome would risk declaring the order cancelled while a subscription that was actually accepted
sits unaccounted for; treating a late success as moot rather than as created would leave that
subscription active and billed with no rollback path, the same stranded-subscription failure mode
the design's own risk register names.

**ADR**: ADR-0005 (`cpt-cf-bss-orders-workflow-adr-saga-compensable-no-pivot`) — cancellation fencing as a precondition of reporting any compensated outcome.

**Propagates to**: `gears/bss/orders-workflow/docs/design/06-saga-and-compensation.md` §
`3.6 Interactions & Sequences` (Five-Step Cancellation Fencing)

### D-31: The FulfillmentTask unilateral-cancel window closes at activation-intent acceptance, not at draft-create acceptance

**Decision**: a `FulfillmentTask` with only an accepted wave-1 draft-create is still unilaterally
cancellable without the fencing machinery, because a `draft` subscription is not
resource-affecting. From activation-intent acceptance onward, the task must be reconciled to a
terminal outcome through the fencing sequence before the order-level outcome is reported.

**Rationale**: fencing exists to guard against a race with an already resource-affecting
provisioning action; requiring the full fencing machinery for a still-draft, non-resource-affecting
subscription would add process overhead with no corresponding race to guard against.

**Propagates to**: `gears/bss/orders-workflow/docs/design/06-saga-and-compensation.md` §
`3.2 Component Model` (Cancellation-Fencing Component, Additional context)

## H. Manual tasks, override, queue, overdue escalation (slice 07)

### D-32: Exactly one actionable manual task is created per permanently failed line under the default remediation policy, from any of the four failed-entrance routes; fail-fast substitutes a non-actionable tracked incident instead

**Decision**: a single creation call site (Manual-Task Creator) is the only entrance to
`ManualTask` creation, invoked identically from all four `failed`-entrance routes — retry
exhaustion, failure confirmation, step-deadline expiry, and sweep-discovered terminal failure —
under the default remediation policy, with dedup against an already-existing task for the same
`FulfillmentTask`. Under fail-fast, a structurally distinct, non-actionable `Incident` is recorded
instead, carrying identifying and failure-reason fields but no SLA deadline or resolution actions.
Together the two paths guarantee 100% failure visibility: every permanent failure produces either
one manual task or one tracked incident, never neither.

**Rationale**: "by any route" is only honest if enforced structurally at one call site rather than
by convention at each entrance, since a task created on only some paths reproduces the
silent-failure bug the visibility requirement exists to prevent; an actionable retry/override/
escalate object is meaningless once the order is already terminal under fail-fast, so a degraded
version of the same object was rejected in favour of a structurally distinct, honestly
non-actionable record.

**ADR**: ADR-0009 (`cpt-cf-bss-orders-workflow-adr-manual-task-dead-letter-separation`) — the one-object-per-failure rule and the manual-task / tracked-incident split.

**Propagates to**: `gears/bss/orders-workflow/docs/design/07-manual-tasks.md` §
`2.2 Any-Route Manual-Task Creation`

### D-33: An override is rejected outright whenever it cannot be verified against a matching, active Subscriptions record; there is no provisional-acceptance path

**Decision**: override acceptance is gated on a synchronous Subscriptions verification call
(active, matching plan, quantity, and tenant), with no provisional-acceptance path. Failure to
verify is a hard rejection, not a soft warning. Operator identity and justification are recorded
in the audit log only on the verified-success branch.

**Rationale**: `OrderCompleted` carries the per-line `subscriptionId` as its audit backbone under
Lifecycle's atomic-completion invariant, so an unverified override would fabricate that linkage;
verification must precede trust rather than trust being extended provisionally and reconciled
later.

**Propagates to**: `gears/bss/orders-workflow/docs/design/07-manual-tasks.md` §
`3.6 Interactions & Sequences` (Override Rejected Without Verified Subscription)

### D-34: The overdue-fulfillment escalation window never auto-terminals the order and never by itself marks any line failed

**Decision**: the Overdue Escalation Monitor's firing is strictly non-terminalling — it raises an
operational escalation to the fulfillment operator queue only. The order remains `in_fulfillment`
(or the `on_hold` state taken from it) until operational compensation reaches a known outcome or an
operator explicitly cancels; the window itself never transitions any line to `failed` and never
auto-terminals the order.

**Rationale**: a subscription-spawn signal may already be in flight when the window elapses, and an
automatic terminal transition on top of that would be unsafe; this is also why Orders Lifecycle
does not auto-expire `in_fulfillment` or holds taken from it, making a non-terminalling escalation
the only safe deadline mechanism available to this gear.

**Propagates to**: `gears/bss/orders-workflow/docs/design/07-manual-tasks.md` §
`2.2 Overdue Window Is Non-Terminalling`

## I. Hold, resilience, and workflow-mediated cancel (slice 08)

### D-35: A hold suspends this gear's own dispatch and timers only; it never pauses, reads, or owns the Subscriptions draft auto-void TTL, and a paused timer resumes with its remaining window, never the full window

**Decision**: `OrderHeld` suspends this gear's own dispatch and timers only. It never pauses,
reads, or assumes the state of the Subscriptions draft auto-void TTL, which is a foreign
aggregate's clock and keeps running through a hold; already-accepted provisioning intents run to
terminal outcome against the frozen plan and are not reversed by a hold. Timer pause stores the
remaining window explicitly rather than re-deriving it, and resume rearms at `resume_time +
remaining_window`, never `resume_time + full_window`. Resume never performs its own wave-1
rebuild — it invokes slice 05's existing pre-activation draft re-read before any activation
intent and defers to slice 05's rebuild mechanism if a voided draft is found, regardless of
whether the hold, the draft TTL, or any other cause voided it.

**Rationale**: "suspend the process" is not the same guarantee as "pause every clock the order is
subject to"; treating the two as equivalent would let a hold silently extend a foreign TTL it does
not own, and re-deriving a remaining window instead of storing it would risk resetting a
partially-elapsed window to full on resume, defeating the guarantee a single hold/resume cycle
must preserve.

**Propagates to**: `gears/bss/orders-workflow/docs/design/08-hold-and-cancel.md` §
`2.1 A Hold Suspends Dispatch, Not Foreign Clocks` and §
`2.2 Timer Pause Preserves Remaining Window`

### D-36: Dependency-retry resilience and the Generic Approval fail-closed park are structurally distinct mechanisms, never one code path branching on dependency name

**Decision**: retry-then-manual-task applies only to Lifecycle/Subscriptions/Payments transient
unavailability, uses the calling step's existing retry budget, and escalates to a manual task on
exhaustion. Generic Approval unavailability (once that service exists) is exclusively slice 03's
fail-closed park: the order stays `submitted`, the Lifecycle `submitted` TTL is not paused, and
escalation happens before that TTL elapses. The two mechanisms are never unified into one code
path that branches on which dependency is unavailable.

**Rationale**: the two mechanisms answer different questions — resilience assumes the dependency
will recover and the process should keep trying, while the fail-closed park assumes no verdict can
be trusted yet and the process must not proceed; collapsing them would risk retrying a park
condition indefinitely or parking on a merely transient outage that resilience should absorb.

**ADR**: ADR-0007 (`cpt-cf-bss-orders-workflow-adr-fail-closed-verdict-park`) — the fail-closed park is the mechanism this entry keeps structurally distinct from dependency retry.

**Propagates to**: `gears/bss/orders-workflow/docs/design/08-hold-and-cancel.md` §
`2.1 Retry-Then-Escalate Is Not the Same Mechanism as Fail-Closed Park`

## J. Read projection and authorization (slice 09)

### D-37: Every system-actor grant requires a gateway-asserted service principal carrying a scope claim naming the calling gear; actor class alone is never sufficient

**Decision**: every system-actor grant (Orders Lifecycle, Generic Approval, Subscriptions,
Payments) requires a gateway-asserted service principal carrying a scope claim naming the calling
gear; actor class alone is insufficient to authorize a call. Generic Approval, Subscriptions, and
Payments are additionally restricted to reporting an outcome and must not drive order-state
transitions directly.

**Rationale**: an actor-class-only check cannot distinguish a legitimately calling gear from any
other caller asserting the same class, which is the unscoped-grant defect the sibling Orders
Lifecycle design's own review found and this design deliberately checked for and avoided by
following that precedent explicitly rather than re-deriving it.

**Propagates to**: `gears/bss/orders-workflow/docs/design/09-read-and-authz.md` §
`2.2 System-actor grants require a gateway-asserted service principal`

### D-38: Process-record retention defaults to at least 400 days at audit grade, configurable, independently of any durable-execution engine's own run-history retention, with the Orders Workflow platform-audit-policy owner as its named executor

**Decision**: process records (saga log, process audit, dead-letter records, manual-task history)
are retained at audit grade for a default of at least 400 days, configurable, independently of any
durable-execution engine's own run-history retention; an engine purge of run history must not
erase this gear-owned audit record. The retention policy's named executor is the Orders Workflow
gear's platform-audit-policy owner, the same role already accountable for the gear's
dead-letter/manual-task audit trail.

**Rationale**: a retention rule with no named executor is a defect since no one is accountable for
enforcing or adjusting it; tying the floor to the gear's own audit-policy owner rather than to
whichever durable-execution engine is eventually selected keeps the audit record's survival
independent of an engine-side purge policy this gear does not control.

**ADR**: ADR-0001 (`cpt-cf-bss-orders-workflow-adr-durable-execution-substrate`) — gear-owned retention is a consequence of the audit-SoR decision.

**Propagates to**: `gears/bss/orders-workflow/docs/design/09-read-and-authz.md` §
`4.3 API latency and retention policy values`

## K. Tuning-value working baselines (engine and intent path)

Every decision in this section is a **working baseline** proposed into the program-wide
non-functional workshop the PRD defers to, not a settled platform value. Each is an engineering
tuning value with no commercial consequence beyond latency and throughput; the commercial-policy
values remain unset and routed, per Q-02.

### D-39: The retry backoff curve is exponential with base 1 s, coefficient 2.0, a 30 s cap and full jitter, over a maximum of 5 submission attempts

**Decision**: the delay before attempt *n* is drawn uniformly from `[0, min(30 s, 1 s * 2^n)]`,
with a maximum of 5 attempts against intent-submission failures only.

**Rationale**: full jitter is required rather than preferred — an unjittered or equal-jittered
retry train from many concurrent orders re-synchronises on the shared Subscriptions path and turns
a transient failure into a self-inflicted load spike. Plain exponential backoff and equal jitter
were both rejected on that ground. Five attempts spends roughly 15-30 s of cumulative backoff,
which keeps the budget provably nested inside the 5 min step deadline of D-41 rather than competing
with it.

**Propagates to**: `design/01-foundation.md` §4.5 (backoff curve and jitter working baseline)

### D-40: The per-attempt timeout is 10 s, set from the downstream's service objective rather than from caller patience

**Decision**: a single attempt is cut at 10 s.

**Rationale**: the standard is 3-10x the downstream's p99, not a figure chosen for caller comfort.
The PRD puts control-operation acceptance at p95 < 1 s, so 10 s leaves headroom for a slow-but-live
dependency while still cutting a hung socket long before it consumes the step deadline. A timeout
chosen from caller patience instead would either abandon healthy slow calls or let one hang absorb
the whole step.

**Propagates to**: `design/01-foundation.md` §4.2 (working baselines for the four bounds)

### D-41: The step deadline is 5 minutes per wave step, derived from the fulfillment service objective rather than chosen

**Decision**: each wave step is bounded at 5 minutes, inclusive of all attempts and backoff.

**Rationale**: this is a derivation, not a preference. The PRD's p95 <= 15 minutes for a standard
order spans two serial waves plus the barrier and the acknowledgement, which leaves roughly 5
minutes per wave step. Choosing a larger step deadline would make the service objective
unsatisfiable by construction; a smaller one would fail healthy slow provisioning.

**Propagates to**: `design/01-foundation.md` §4.2 (working baselines for the four bounds)

### D-42: The nesting invariant over the four bounds is asserted at configuration load, and a violating configuration is refused at startup

**Decision**: per-attempt timeout **<** step deadline **<** process deadline must hold, and a
configuration violating that ordering is refused at startup rather than accepted.

**Rationale**: a mis-ordered set does not fail loudly — it silently disables the inner bound, and
nothing else in the engine detects it. Treating the ordering as an asserted invariant rather than
as documentation is what makes the four-bound model in D-02 enforceable instead of aspirational.

**Propagates to**: `design/01-foundation.md` §4.2 (the nesting invariant is normative)

### D-43: A gear-wide retry budget caps retries at 10% of request volume with adaptive throttling, in addition to the per-request attempt cap

**Decision**: the retry/backoff controller enforces a gear-wide retry share, working baseline 10%
of request volume over a sliding window, with adaptive client-side throttling beyond it. The
remaining step budget is propagated on every outbound call rather than each hop timing out
independently.

**Rationale**: per-request attempt caps alone do not prevent amplification. Under a sustained
downstream failure every in-flight order retrying five times multiplies offered load at exactly
the moment the dependency is weakest, so the aggregate rate must degrade rather than compound.
Deadline propagation was adopted with it because without it Subscriptions keeps working on
requests this gear has abandoned, widening the late-success window slice 06's fencing must then
clean up.

**Propagates to**: `design/01-foundation.md` §4.5 (a retry budget bounds the gear, not just the request)

### D-44: Per-order line parallelism is 8; the cross-process aggregate limit is sized by Little's Law from measured capacity and preferably adaptive; the dead-letter delivery cap is 5

**Decision**: 8 parallel lines per order; the aggregate in-flight-intent limit carries the working
baseline **256 concurrent intents gear-wide**, to be replaced by the `L = lambda * W` derivation
from measured downstream throughput and latency once measured capacity exists, with a gradient- or
delay-based adaptive controller preferred over a static ceiling thereafter; the admission queue in
front of it is capped at **10 x the aggregate limit (2,560 entries), reject-on-full**; per-tenant
token bucket beneath a global semaphore, keyed on `seller_tenant_id`; dead-letter delivery cap
of 5.

**Rationale**: a guessed aggregate constant is either wasteful or a bottleneck as Subscriptions'
capacity moves and cannot distinguish the two cases, so the limit is ultimately a derivation from
measurement rather than a number. A blank is worse than a baseline, however: with no value at all
there is nothing to configure, nothing to load-test against, and nothing for the queue-depth cap to
be ten times of. 256 is therefore stated as a working baseline with its derivation — 32 concurrent
orders at the per-order parallelism of 8 — explicitly placeholding for the Little's-Law figure and
carrying no claim to be the measured answer. A delivery cap of 5 absorbs ordinary at-least-once redelivery
without parking a payload a brief blip would have cleared.

**Propagates to**: `design/01-foundation.md` §4.12 (concurrency and back-pressure working baselines), §4.8 (delivery-count cap)

### D-45: The reconciliation-sweep ladder is 30 s to 1 h with jitter and a bounded page size, and must resolve every intent inside the overdue window

**Decision**: escalating re-reads at 30 s, 1 min, 2 min, 5 min, 15 min, 1 h, capped hourly, each
wake-up jittered, each pass bounded by page size; the ladder must reach a terminal outcome or the
dead-letter floor within the 24 h overdue window.

**Rationale**: the window bound is a correctness constraint, not tuning — a schedule still
re-reading past 24 h would let the overdue escalation fire on an intent the sweep had not resolved.
Jitter is required because the sweep runs across every non-terminal intent at once, so a fixed
interval re-reads them in lockstep and the sweep becomes the load spike it exists to recover from.

**Superseded in part by D-52**, which splits this ladder in two: wave-2 intents, which sit inside
the measured fulfillment window, need a faster in-window ladder, because the single ladder here
reaches its hourly cap 23.5 minutes in — slower than the 15-minute p95 it is offered as the
recovery for. The schedule stated above is retained for every intent outside that window.

**Propagates to**: `design/05-provisioning-intents.md` §4.2 (reconciliation-sweep schedule)

### D-46: Generic Approval outage detection is fixed with a circuit breaker, while the escalation threshold remains deliberately unset

**Decision**: the verdict gateway detects unavailability through a circuit breaker opening at a
50% failure rate over a sliding window of 20 calls, held open 60 s, then probed half-open. The
threshold at which a parked process becomes an operator-visible incident is stated
**relationally**, not as an absolute: `min(30 min, 0.25 x lifecycle_submitted_ttl)`, with an
escalation lead time before the TTL of `max(4 h, 0.25 x lifecycle_submitted_ttl)`. Both are
**Accepted**. `lifecycle_submitted_ttl` is read from configuration and
asserted at startup; no absolute number is fixed on this side.

**Rationale**: these are two different quantities that have been conflated elsewhere. Detection
carries no commercial consequence — it determines only how quickly this gear notices. The
escalation threshold trades directly against the Orders Lifecycle `submitted` TTL, which this gear
does not own, and the binding requirement is relational rather than numeric: escalation must fire
before that TTL elapses. A number fixed on this side would either assume a TTL this gear cannot
see or become the platform's de facto commercial answer — so the threshold is expressed as a
function of that TTL instead, which is implementable today and still leaves the commercial call to
Product. Expressing it relationally is not the same as setting it: the multipliers and floors
above still require sign-off, and the TTL itself is an open upstream ask on Orders Lifecycle. The 20-call window is narrower than the
common 100-call default because per-gate verdict-query volume is low and a 100-call window would
not open until long after the dependency was plainly unavailable.

**Propagates to**: `design/03-approval-execution.md` §4.0 (detecting a Generic Approval outage, and what this design will not set)

## L. Cross-cutting decisions (remediation)

The decisions in this section were taken while remediating the design set against review. Each one
crosses more than one slice, which is why it is recorded here rather than under a slice letter.
Each entry carries a stated value with its derivation. Eight of them were raised as proposals
rather than decisions, because they are commercial judgements — what the business promises a
customer, or what an operator is permitted to do — rather than engineering ones. They were carried
through review under an explicit sign-off marker so that none could be mistaken for a settled
number. All eight have since been accepted and are marked **Accepted** below; the derivations are
retained so a later reader can argue with the value rather than guess at where it came from.

### D-47: Idempotency keys form four tenant-prefixed families, and `gateId` is derived rather than minted

**Decision**: every idempotency key in this gear is prefixed with `resource_tenant_id`, so key
spaces are tenant-namespaced and two tenants can never collide. Four families exist and no fifth is
invented:

| Family | Composition |
|---|---|
| Provisioning intent | `orderId` + `orderVersion` + `orderLineId` + `wave` + `intentKind` |
| Provisioning intent, wave-1 rebuild | the five above plus `attempt` (integer from 1, persisted on `owf_provisioning_intent.wave_attempt`) |
| Approval request | `orderId` + `orderVersion` + `gateId` |
| Lifecycle transition call | `orderId` + `orderVersion` + `transitionName`, where `transitionName` is one of the five seam operations |

`gateId` is **derived deterministically** as a UUIDv5 over `(orderId, orderVersion, party)`, never
minted as a random uuid.

**Rationale**: a randomly minted `gateId` breaks approval idempotency at exactly the moment it
matters — a crash between submitting to Generic Approval and committing the gate row mints a
different id on replay, hence a different key, hence a second gate for the same party on the same
order version. Deriving it makes the replay converge. The Lifecycle transition-call family exists
because reflections across that seam are retried and must be absorbed rather than double-applied,
and no other family's shape fits an order-scoped call with no line, wave or kind. The tenant prefix
is the cheapest way to make cross-tenant collision structurally impossible rather than statistically
unlikely.

**ADR**: ADR-0006 (`cpt-cf-bss-orders-workflow-adr-idempotency-key-composition`) — the full key
composition, including this family set.

**Propagates to**: `gears/bss/orders-workflow/docs/design/03-approval-execution.md` §3.7
(approval-gate schema and key), `gears/bss/orders-workflow/docs/design/05-provisioning-intents.md`
§3.7 (intent schema and key), `gears/bss/orders-workflow/docs/design/06-saga-and-compensation.md`
§2.1 (compensating-intent keys)

### D-48: Three tenant axes, and every gear-owned table carries at least the resource axis

**Decision**: this gear adopts the sibling Orders Lifecycle set's three tenant axes by name —
`resource_tenant_id` (resource recipient), `payer_tenant_id` (billing party), `seller_tenant_id`
(selling party). Every one of the gear's `owf_*` tables carries **at least** `resource_tenant_id`,
`NOT NULL`. A table backing an operator- or seller-scoped surface additionally carries
`seller_tenant_id`. Each table states its axis choice in its **Additional info** line. Per-tenant
fairness and back-pressure key on `seller_tenant_id`.

**Rationale**: "every table is tenant-scoped" was asserted in the foundation slice while no table
carried a tenant column, which leaves the platform's SecureORM `#[secure(tenant_col = ...)]`
isolation with nothing to attach to and the read slice's "every read is tenant-scoped" with no
enforcing predicate on the audit, step-log, dead-letter, timer or idempotency stores. One axis is
not enough either: the party who receives the resource, the party who pays, and the party who sells
are three different tenants on a reseller order, and collapsing them would silently mis-scope
either the operator surfaces or the billing joins. Adopting the sibling's names rather than
inventing new ones keeps cross-gear joins and operator tooling consistent.

**Propagates to**: `gears/bss/orders-workflow/docs/design/01-foundation.md` §3.7 (all engine
tables), and the §3.7 of every slice that declares a table; `DESIGN.md` §3.7 (table registry)

### D-49: Optimistic concurrency is a row version surfaced as an ETag, and one active instance per order is a partial unique index

**Decision**: `owf_process_instance` and `owf_manual_task` each carry
`row_version bigint NOT NULL DEFAULT 0`, incremented on every write, surfaced to callers as an
ETag and **required** as `If-Match` on the mutating operations the PRD marks "Optimistic … version
check REQUIRED". A mismatch is a `409` in the RFC-9457 problem envelope. Separately,
`owf_process_instance` carries `UNIQUE (order_id) WHERE terminal_outcome IS NULL`.

**Rationale**: the PRD requires the version check and no column, parameter or status code existed
to implement it, so two operators resolving the same task both committed and a cancel racing an
approval advance both committed. The partial unique index closes the matching hole on the other
side: "exactly one active instance per order" was specified as a read-then-insert admission check,
which two workers consuming a redelivered `OrderSubmitted` both pass, producing two instances, two
verdict queries and two Lifecycle reflections. A uniqueness constraint the database enforces is the
only form of that rule that survives concurrency; the admission read stays as a fast path, not as
the guarantee.

**Propagates to**: `gears/bss/orders-workflow/docs/design/01-foundation.md` §3.7
(`owf_process_instance`), `gears/bss/orders-workflow/docs/design/07-manual-tasks.md` §3.7
(`owf_manual_task`), `gears/bss/orders-workflow/docs/design/09-read-and-authz.md` §3.3 (`If-Match`
on mutating operations)

### D-50: The audit log is append-only and hash-chained, and human justification is a separate column from the reason code

**Decision**: `owf_audit_entry` is append-only with **no UPDATE or DELETE grant** and is
**hash-chained**: each row carries `prev_hash` and `entry_hash`, where `entry_hash` covers the
row's own content together with `prev_hash`, so a removed or altered row breaks the chain at a
verifiable point. A free-text `justification text` column carries human-supplied reasons (override
justification, cancellation reason); it is distinct from `reason`, which stays a closed catalogue
value and is the only one of the two that rides event payloads.

**Rationale**: "append-only" enforced by convention is a discipline, not a property — a compliance
grade audit trail retained for 400 days needs tamper-evidence a reviewer can check without trusting
the writer, which is what the chain provides, and it matches the sibling Lifecycle set's own audit
posture rather than inventing a weaker one. Splitting `justification` from `reason` is what keeps
the reason catalogue closed: operators need to say *why* in prose, consumers need to key on a
stable enum, and one column cannot do both without either breaking consumers or censoring
operators.

**ADR**: ADR-0001 (`cpt-cf-bss-orders-workflow-adr-durable-execution-substrate`) — the audit log is
the gear-owned source of record this hardens.

**Propagates to**: `gears/bss/orders-workflow/docs/design/01-foundation.md` §3.7
(`owf_audit_entry`) and §4.6 (audit completeness)

### D-51: `parked` is a distinct process-phase value

**Decision**: the process phase enum is `started | suspended | parked | compensating |
terminated`, with declared transitions, defined in exactly one place. `parked` is the "verdict
unobtainable" state ADR-0007 requires: a *process* phase, not an order state — the order itself
remains `submitted` — and distinct from `suspended`, which records an operator- or
caller-initiated hold. A process is never in both.

**Rationale**: ADR-0007 records the park as a required consequence and nothing implemented it: the
verdict cache admitted only `required` / `not_required`, the phase enum had no park value, and no
table, sequence branch or timer kind existed for a parked order — which left the one acceptance
criterion that applies today unsatisfiable in all four of its bullets. Reusing `suspended` was
rejected because hold and park are entered by different actors, escalate on different clocks
(gate escalation timer vs. Lifecycle `submitted` TTL) and are left by different events; one value
for both would make every query that asks "why is this order not moving" unanswerable.

**ADR**: ADR-0007 (`cpt-cf-bss-orders-workflow-adr-fail-closed-verdict-park`) — the park this value
implements.

**Propagates to**: `gears/bss/orders-workflow/docs/design/01-foundation.md` §3.7
(`owf_process_instance.phase`), `gears/bss/orders-workflow/docs/design/03-approval-execution.md`
§3.6 (the park branch)

### D-52: The reconciliation-sweep ladder splits by phase

**Decision**: the sweep runs two ladders, selected by what it is reconciling. Wave-2 intents, which
sit inside the measured fulfillment window, use **5 s → 15 s → 30 s → 60 s → 2 min, terminal by
t+5 min**. Every other intent keeps the existing ladder, **30 s → 1 m → 2 m → 5 m → 15 m → 1 h**.
Both jitter every wake-up, both page at 500 rows under a 30 s transaction budget, and the sweep
floor is **30 reads or 23 h, whichever comes first** — a separate quantity from the inbound
delivery cap of 5.

**Rationale**: the single ladder reaches its hourly cap 23.5 minutes in, which is slower than the
15-minute p95 it was offered as the design response to. Because a post-accept hang is excluded from
the retry budget, the sweep is the *only* recovery for a lost wave-2 confirmation, so a ladder
slower than the SLA guarantees the SLA is missed whenever the sweep is what saves the order. The
in-window ladder is derived from that budget rather than chosen. The existing ladder is kept for
everything outside the window because re-reading every non-terminal intent every 5 seconds makes
the sweep the load spike it exists to recover from.

**Propagates to**: `gears/bss/orders-workflow/docs/design/05-provisioning-intents.md` §4.2
(reconciliation-sweep schedule); supersedes the single-ladder statement in D-45

### D-53: A non-pausable `max_process_lifetime` of 90 days bounds every process, independently of order state

**Accepted.**

**Decision**: an unconditional, non-pausable `max_process_lifetime = 90 days` is armed at process
start and is independent of order state, hold, resume or approval. There is **no cap on hold/resume
cycles**; this timer is the bound instead. On expiry the process escalates; it never silently
terminates an order.

**Rationale**: the previously adopted backstop — the overdue-fulfillment escalation window — only
runs while the order is `in_fulfillment`, so it bounds nothing for an order cycling between hold
and resume before fulfillment starts, which is precisely the case Q-03 asks about. Capping cycles
was rejected because a legitimate commercial negotiation can involve many holds and the cap would
refuse the last one for no reason the customer can see. A wall-clock lifetime armed once at start
is immune to cycling by construction. 90 days is stated as a value with a derivation — an order
non-terminal for a full quarter is an operational fact somebody must look at. The figure was
carried as a proposal through review and has since been accepted as the settled value.

**Propagates to**: `gears/bss/orders-workflow/docs/design/08-hold-and-cancel.md` §3.6 (hold and
resume), `gears/bss/orders-workflow/docs/design/01-foundation.md` §4.2 (bounds); resolves the
interim backstop recorded under Q-03

### D-54: A partial wave-2 failure holds the order; already-activated lines are not rolled back

**Accepted.**

**Decision**: when some wave-2 activations succeed and others fail permanently, the order is
**held**. Already-activated lines are **not** rolled back. The order cannot be acknowledged
`completed` — atomic fulfillment forbids it — and it enters the remediate policy. The only path to
a terminal outcome from there is an **operator-initiated, workflow-mediated cancel**, which
compensates every created subscription under D-27's ordering.

**Rationale**: the alternatives are worse in ways a customer feels. Rolling back the successful
lines destroys live, working service to punish an unrelated line's failure. Acknowledging
`completed` lies about an order whose lines are not all activated. Leaving it silently in flight is
the current behaviour and produces four of five lines live and billable on an order no state
machine can ever finish. Holding it makes the partial commercial outcome explicit and puts a human
on it, which is the only honest resolution while the failed line is still remediable.

**Propagates to**: `gears/bss/orders-workflow/docs/design/04-fulfillment-plan.md` §3.6 (wave-2
partial failure), `gears/bss/orders-workflow/docs/design/07-manual-tasks.md` §3.6 (remediate
policy)

### D-55: "Remediation exhausted" is three failed attempts on the same task, or the manual-task SLA deadline elapsing

**Accepted.**

**Decision**: remediation is exhausted at **3 failed operator resolution attempts on the same
task**, or when the **manual-task SLA deadline elapses without resolution** — whichever comes
first. Exhaustion is what triggers order-level compensation under the remediate policy.

**Rationale**: the trigger for order-level compensation had no definition at all, which made the
entire remediate path terminate on an undefined condition — an order either compensated on a
condition nobody could state or never compensated. Two clauses rather than one are needed because
the failure modes are different: an operator who tries three times is facing something operator
action cannot fix, and a task nobody touches before its deadline is facing nobody at all. Either
way the order stops waiting.

**Propagates to**: `gears/bss/orders-workflow/docs/design/07-manual-tasks.md` §3.6 (resolution
paths), `gears/bss/orders-workflow/docs/design/06-saga-and-compensation.md` §3.6 (entry conditions
for order-level compensation)

### D-56: The submitting identity is refused at the approval-decision endpoint

**Accepted.**

**Decision**: the identity that submitted an order is **refused** at that order's
approval-decision endpoint. Separation of duties is added as clause (g) to the §9.2 expectations
contract this gear holds against the Generic Approval service, so routing-side enforcement is
expected upstream as well as refused locally.

**Rationale**: the PRD lets an Approver be a seller operator, and nothing anywhere in the set —
PRD, DESIGN, this register, any ADR or slice — mentions separation of duties, self-approval,
four-eyes or dual control. An actor can therefore submit an order and approve their own gate with
every stated control passing, which defeats the reason the financial/legal/partner gate exists.
Routing is legitimately the approval service's concern, but this gear owns the decision endpoint
and the expectations contract, so the local refusal is the part it can guarantee today and the
contract clause is how it asks for the rest.

**Propagates to**: `gears/bss/orders-workflow/docs/design/03-approval-execution.md` §3.3 (decision
endpoint), `gears/bss/orders-workflow/docs/design/09-read-and-authz.md` §4.1 (permission matrix);
recorded as a PRD amendment ask in `UPSTREAM_REQS.md` §4

### D-57: `lifecycle_submitted_ttl` is mirrored as local configuration under a startup refusal, pending the upstream field

**Accepted.**

**Decision**: this gear holds `lifecycle_submitted_ttl` as its own configuration value, deployed in
the same promotion that carries Orders Lifecycle's TTL policy, and **refuses startup** if the value
is absent, zero or negative, or if `outage_escalation_threshold < escalation_lead_time <
lifecycle_submitted_ttl` does not hold. When Orders Lifecycle exposes the per-order expiry instant
(`cpt-cf-bss-orders-workflow-upreq-submitted-ttl-visibility`), `submitted_expires_at` supersedes the
mirror and only the source of the deadline changes.

**Rationale**: the escalation obligation in `ADR/0007` is a strict inequality against a value this
gear does not own, so it was previously unconfigurable — and an unconfigurable escalation is not a
deferred feature, it is a silent one: a parked order reaches expiry with no human alerted, which is
the fail-closed park failing from the other direction. A mirrored constant carries a real drift
risk, since Lifecycle can retune `orders_state_ttl_policy` without this gear knowing. That risk was
accepted because it is **loud where the alternative was silent**: a drifted mirror is an
operational defect, whereas an absent value was an escalation path that existed in the design and
never ran. The presence-and-positivity check is what converts the remaining failure mode from
silent inertness into a refusal at configuration load, following the nesting-invariant pattern
`01-foundation.md` §4.2 already establishes.

**Consequence**: the upstream ask stays open — this interim does not close it — but it is no longer
a blocker. The park, its timer and its operator escalation are fully operable today.

**Propagates to**: `gears/bss/orders-workflow/docs/design/03-approval-execution.md` §4.2 (the
configuration values and the startup nesting check),
`gears/bss/orders-workflow/docs/ADR/0007-cpt-cf-bss-orders-workflow-adr-fail-closed-verdict-park.md`
(the escalation obligation the inequality serves); the upstream field stays registered as
`UPSTREAM_REQS.md` `…-upreq-submitted-ttl-visibility`

**Propagates to**: `design/03-approval-execution.md` §4.2; `UPSTREAM_REQS.md` §2.4.

**ADR**: `cpt-cf-bss-orders-workflow-adr-fail-closed-verdict-park`.

## Open Questions

### Q-01: Which durable-execution substrate backs the process — the OSS Workflow Engine or a BSS-local mechanism?

**Owner**: Architecture.

**Blocked**: no slice's saga-step, durable-timer, or engine-history-isolation implementation can
be finalized until the substrate is chosen; evaluation must include which commercial data would
sit in engine-side history, isolation and retention of that history, BSS/OSS boundary
compatibility, and the requirement that gear-owned process audit stays independent of engine
purge. Target date 2026-09-30 per `gears/bss/orders-workflow/docs/PRD.md` § `15. Open Questions`.

### Q-02: The Generic Approval escalation threshold — the one PRD-deferred numeric value this design deliberately leaves unset

**Owner**: Product.

**Narrowed**: the engineering half of this question is now closed. The retry backoff curve, maximum
attempts, per-attempt timeout, step deadline, nesting-invariant enforcement, gear-wide retry
budget, per-order concurrency, reconciliation-sweep ladders (D-45 as split by D-52), and
dead-letter delivery cap are fixed as working baselines in D-39 through D-46 and D-52, each with
its derivation, and each proposed into the program-wide non-functional workshop rather than
asserted as settled. Generic Approval outage **detection** is likewise fixed (D-46). The
**aggregate** in-flight cap is the one baseline D-44 originally declined to state; it now carries
the placeholder 256 concurrent intents with its derivation, explicitly standing in for the
Little's-Law figure until measured capacity exists, so the queue-depth cap and the load test have
something to be configured against.

**Still open**: the numeric sign-off on the threshold at which a fail-closed park becomes an
operator-visible incident. The *form* is no longer open — D-46 and ADR-0007 now state it
relationally against the Orders Lifecycle `submitted` TTL, as `min(30 min, 0.25 x
lifecycle_submitted_ttl)`, with an escalation lead time of `max(4 h, 0.25 x
lifecycle_submitted_ttl)`, and both multipliers are now accepted. What remains outstanding is not a
decision but a dependency: Orders Lifecycle still owes visibility of the TTL itself, raised as an
upstream ask in `UPSTREAM_REQS.md`. A bare number chosen on this side would either assume a TTL
value this gear cannot see or silently become the platform's commercial answer, which is why the
relational form is the answer rather than a placeholder for one.

**Blocked**: the threshold cannot be *configured* until the Lifecycle TTL is readable, and has
nothing to apply to until a Generic Approval service exists (Q-01 and the phase-1 inertness
disclosure). The working baselines in D-39 through D-46 and the cross-cutting decisions in section
L are implementable now, subject to confirmation or adjustment by the non-functional workshop.

### Q-03: Does a bounded hold/resume cycle count or bound the total elapsed non-terminal lifetime of an order, beyond the single-cycle remaining-window guarantee this design already keeps?

**Owner**: Product (the fulfillment-operator/product owner named in the PRD's overdue-escalation
rationale).

**Resolved in form, open on the value**: D-53 answers the mechanism — no cap on cycles, and an
unconditional non-pausable `max_process_lifetime` of 90 days armed at process start, and the
90-day figure is accepted. The previously recorded interim backstop, the
overdue-fulfillment escalation window (slice 07), is **withdrawn as a backstop**: it only runs
while the order is `in_fulfillment`, so it bounds nothing for an order cycling between hold and
resume before fulfillment begins, which is the case this question actually asks about. What remains
open is nothing on this gear's side; this mirrors the same hardening item the
sibling Orders Lifecycle design set records for its own hold/expiry surface.

### Q-04: The SUB-O renumbering and the missing OrderAmended re-verdict requirement — two required PRD amendments

**Owner**: Architecture.

**Blocked**: PRD §13 and its in-body citations still read the pre-fork `SUB-O6`–`SUB-O9` numbers,
which collide with the canonical `SEAMS.md` register's own, unrelated `SUB-O6`; until the PRD is
amended to cite `SUB-O11`–`SUB-O14`, any reader working from the PRD alone will misidentify which
ask is which. Separately, PRD §6.2 does not yet state that `OrderAmended` must re-obtain the
approval-requirement verdict for the new order version and reflect it onward from `submitted`;
until that one-to-two-sentence addition lands, an amended order's approval requirement is
underspecified at the PRD level even though this design's approval-execution slice already assumes
the re-verdict behaviour.

### Q-05: The Generic Approval service has no specification anywhere in this repository, leaving the whole approval capability inert in phase 1

**Owner**: Architecture.

**Blocked**: multi-party gates, escalation beyond the phase-1 stand-in, the Approver Inbox, and
acceptance criteria #1–#4a are inert until a Generic Approval spec exists; this design's phase-1
stand-in (returning "approval not required", audited) is recorded as the deciding authority by
name only as an interim measure, not as a second policy author, and cannot be extended until the
spec lands.

### Q-06: Payments has no specification and no register, so the payment-authorization ask has no owner; only "provision first, collect after" is expressible

**Owner**: Architecture, with Product.

**Blocked**: the payment-authorization-outcome ask this gear raises has no receiving register to
land in, since no Payments capability spec exists anywhere in this repository. As a consequence,
only the provision-then-collect payment ordering is expressible in this design; a self-service
card checkout that collects before provisioning — with its own capture, SCA-challenge wait,
retry-with-another-instrument, and refund-as-reversal needs — has no home in the current flow and
cannot be implemented against this design until a Payments spec exists and the ordering, outcome
visibility, and reversal-artifact open questions in the PRD are resolved.

### Q-07: Tension between asynchronous outbox publication and the PRD's p95 < 30 s process-event delivery target

**Owner**: Architecture.

**Blocked**: process events publish asynchronously from an outbox row written alongside the audit
entry and drained to the platform event bus on a schedule this design does not yet fix; until the
drain cadence and its own latency budget are specified, the p95 < 30 s delivery target is stated as
a threshold this design must meet but has no committed mechanism yet shown to meet, which the
durable-execution substrate decision (Q-01) and the NFR workshop (Q-02) both bear on.

### Q-08: SUB-O5 (overlap-scope-key presence read) is unagreed, leaving the pre-activation overlap check unevaluable; SUB-O1 (compensation cancellation reason) is critical and unagreed, leaving the activated-cancel reason unspecifiable from this side

**Owner**: Architecture (joint ask toward Subscriptions).

**Blocked**: until `SUB-O5` lands, an unevaluable pre-activation overlap read is treated as a
collision by this design rather than allowed to pass silently, but the overlap check itself cannot
be evaluated as designed until the read exists upstream. Until `SUB-O1` lands, the activated-cancel
compensation leg has no cancellation-reason value to send, since reason values ride event payloads
consumers key on and adding one after Billing consumes the contract would be a breaking change;
this design names the requirement and defers the value rather than inventing a placeholder.

## What This Design Set Does Not Claim

Following the sibling Orders Lifecycle design set's own precedent: the coherence of this design
set — that its nine `design/` slices and this `DECISIONS.md` register agree with each other and
with the PRD — is asserted here by reviewer-checkable statements, not enforced by CI. This is
because `.cf-studio/config/artifacts.toml` excludes `docs/design/*.md` and `DECISIONS.md` from
`cfs` autodetect, and `cfs` hardcodes its artifact-kind set, so a `DESIGN_SLICE` kind cannot be
registered for either document type. Only `DESIGN.md`, `ADR/*.md`, and `UPSTREAM_REQS.md` are
gate-enforced by `cfs validate --artifact` for this gear. Nothing in this register should be read
as CI-verified; every propagation address above was checked by hand against the heading it names,
and that hand-check is the only guarantee this document offers.


## Traceability

| Decision | Slice | Consuming document |
|----------|-------|---------------------|
| D-01–D-04 | 01 Process engine | `design/01-foundation.md` |
| D-05–D-08 | 02 Triggers and start | `design/02-triggers-and-start.md` |
| D-09–D-14 | 03 Approval execution | `design/03-approval-execution.md` |
| D-15–D-18 | 04 Fulfillment plan | `design/04-fulfillment-plan.md` |
| D-19–D-20 | Commercial policy (owned elsewhere) | `PRD.md` § 15 |
| D-21–D-26 | 05 Provisioning intents | `design/05-provisioning-intents.md` |
| D-27–D-31 | 06 Saga and compensation | `design/06-saga-and-compensation.md` |
| D-32–D-34 | 07 Manual tasks | `design/07-manual-tasks.md` |
| D-35–D-36 | 08 Hold and cancel | `design/08-hold-and-cancel.md` |
| D-37–D-38 | 09 Read and authorization | `design/09-read-and-authz.md` |
| D-39–D-40 | K Tuning baselines (engine) | `design/01-foundation.md` §4.2, §4.5 |
| D-41–D-42 | K Tuning baselines (engine) | `design/01-foundation.md` §4.2 |
| D-43 | K Tuning baselines (engine) | `design/01-foundation.md` §4.5 |
| D-44 | K Tuning baselines (concurrency) | `design/01-foundation.md` §4.12, §4.8 |
| D-45 | K Tuning baselines (intent path) | `design/05-provisioning-intents.md` §4.2 |
| D-46 | K Tuning baselines (approval path) | `design/03-approval-execution.md` §4.0 |
| D-47 | L Cross-cutting (keys) | `design/03-approval-execution.md` §3.7, `design/05-provisioning-intents.md` §3.7, `design/06-saga-and-compensation.md` §2.1 |
| D-48 | L Cross-cutting (tenancy) | every slice §3.7; `DESIGN.md` §3.7 table registry |
| D-49 | L Cross-cutting (concurrency control) | `design/01-foundation.md` §3.7, `design/07-manual-tasks.md` §3.7, `design/09-read-and-authz.md` §3.3 |
| D-50 | L Cross-cutting (audit) | `design/01-foundation.md` §3.7, §4.6 |
| D-51 | L Cross-cutting (process phase) | `design/01-foundation.md` §3.7, `design/03-approval-execution.md` §3.6 |
| D-52 | L Cross-cutting (sweep) | `design/05-provisioning-intents.md` §4.2 |
| D-53 | L Cross-cutting (lifetime bound) | `design/08-hold-and-cancel.md` §3.6, `design/01-foundation.md` §4.2 |
| D-54 | L Cross-cutting (partial failure) | `design/04-fulfillment-plan.md` §3.6, `design/07-manual-tasks.md` §3.6 |
| D-55 | L Cross-cutting (remediation) | `design/07-manual-tasks.md` §3.6, `design/06-saga-and-compensation.md` §3.6 |
| D-56 | L Cross-cutting (separation of duties) | `design/03-approval-execution.md` §3.3, `design/09-read-and-authz.md` §4.1 |
| D-57 | K Tuning baselines (approval path) | `design/03-approval-execution.md` §4.2, `ADR/0007-cpt-cf-bss-orders-workflow-adr-fail-closed-verdict-park.md`, `UPSTREAM_REQS.md` (`…-upreq-submitted-ttl-visibility`) |

Highest decision number used: **D-57**. Numbering is one continuous sequence across the whole
register; there are no parts.
