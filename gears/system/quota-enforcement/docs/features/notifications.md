<!-- Created: 2026-08-24 by Constructor Tech -->

# Feature: Notification Outbox & Dispatch

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-featstatus-notifications-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-quota-enforcement-feature-notifications`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Sink Registration and Event Delivery](#sink-registration-and-event-delivery)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Leased-Handler Dispatch Cycle](#leased-handler-dispatch-cycle)
  - [Threshold-Crossed Emission Semantics](#threshold-crossed-emission-semantics)
- [4. States (CDSL)](#4-states-cdsl)
  - [Outbox Event State Machine](#outbox-event-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Sink Plugin Contract](#sink-plugin-contract)
  - [Dispatcher as Outbox Leased Handler](#dispatcher-as-outbox-leased-handler)
  - [Event Catalog Conformance](#event-catalog-conformance)
  - [Dispatch Telemetry](#dispatch-telemetry)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Implements the `QuotaNotificationSinkV1` plugin contract and the outbox-backed dispatcher that delivers the eight-kind
event catalog at-least-once to every registered sink, with per-sink failure isolation, retry and dead-letter policy,
and framework-leased dispatch.

### 1.2 Purpose

Every mutating feature enqueues events into the `notification_outbox` in the same transaction as its state change
(invariant I11, delivered by foundation's storage contract). This feature drains that queue: without it, threshold
alerts, period rollovers, and lifecycle events never leave the gear. Delivery is best-effort by requirement — a dead
sink must never block enforcement writes.

**Scope**: the sink plugin trait and its closed `DispatchError` contract, the dispatcher as a `toolkit-db` Outbox
**leased handler** (the framework owns claiming, redelivery, acks, dead letters, and lease fencing), the event catalog
payloads and discriminators, threshold upward-transition semantics, the retry policy with explicitly permitted
duplicates, and dispatch telemetry.

**Out of scope**: producing events (each mutating feature enqueues its own, same-tx via the storage contract), the
outbox tables and I11 guarantee (foundation, via the `toolkit-db` Outbox), the cluster coordination adapter (sweeper
singletons only — the dispatcher is fenced by the Outbox lease), EventBus routing (P2 per PRD §13), and any QE-side subscription
primitive (P2; P1 sinks filter on `event.tenant_id` themselves).

**Requirements**: `cpt-cf-quota-enforcement-fr-notification-plugin`

**Principles**: None introduced; delivery rides foundation's storage contract and coordination adapter.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-notification-sink` | Deployment-specific plugin receiving every dispatched event |
| `cpt-cf-quota-enforcement-actor-platform-operator` | Registers sink implementations at deployment; watches dead-letter telemetry |
| `cpt-cf-quota-enforcement-actor-monitoring-system` | Scrapes dispatch-failure and outbox-backlog instruments |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-foundation`

## 2. Actor Flows (CDSL)

**Use cases**: None declared in the PRD for this surface; the flow below is the operator-visible lifecycle.

### Sink Registration and Event Delivery

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-sink-delivery`

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator`

**Success Scenarios**:
- Registered sinks receive every catalog event at least once, in enqueue order per drain batch

**Error Scenarios**:
- No sink registered: bootstrap surfaces the telemetry warning; the handler acks each event unprocessed — events are
  dropped silently per the PRD §11 assumption, and sink registration requires deployment
- A sink times out or fails transiently: the delivery retries with backoff; other sinks are unaffected
- A sink fails permanently: the handler returns `Reject` for the whole event and the framework moves it to the
  dead-letter store (P1 has no per-sink dead letter); writes never block

**Steps**:
1. [ ] - `p1` - Operator registers `QuotaNotificationSinkV1` implementations at deployment - `inst-del-register`
2. [ ] - `p1` - **IF** no sink is registered at bootstrap - `inst-del-nosink-if`
   1. [ ] - `p1` - Surface the "no notification sinks registered" telemetry warning and continue serving - `inst-del-nosink-warn`
3. [ ] - `p1` - A mutating operation commits; its events are already in `notification_outbox` same-tx (I11) - `inst-del-enqueue`
4. [ ] - `p1` - DB: the `toolkit-db` Outbox processor claims a batch under a DB lease and invokes the QE leased
   handler with the dispatcher's system-level `SecurityContext` - `inst-del-claim`
5. [ ] - `p1` - API: the handler fans each event out to every registered sink concurrently, passing the system
   `SecurityContext`, each call bounded by the per-sink timeout (reference default 2 s) - `inst-del-fanout`
6. [ ] - `p1` - **IF** any sink answers `Timeout` or `Transient` - `inst-del-retry-if`
   1. [ ] - `p1` - The handler returns retry; the framework re-delivers the event to **all** sinks later — duplicate
      delivery is permitted and sinks tolerate it per contract - `inst-del-retry`
7. [ ] - `p1` - **IF** any sink answers `Permanent`, or `OutboxMessage.attempts` has reached the operator-configured
   maximum with sinks still transient - `inst-del-reject-if`
   1. [ ] - `p1` - The handler returns `Reject(reason)`; the framework moves the event to its dead-letter store and
      operators replay via `dead_letter_replay` (re-delivery to all sinks; duplicates tolerated) - `inst-del-dead`
8. [ ] - `p1` - **IF** every sink answered `Success` - `inst-del-term-if`
   1. [ ] - `p1` - The handler acks the event - `inst-del-ack`
9. [ ] - `p1` - **RETURN** delivery is at-least-once: a lease that expires mid-dispatch drops the handler future and
   another processor re-claims the batch - `inst-del-alo`

## 3. Processes / Business Logic (CDSL)

### Leased-Handler Dispatch Cycle

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-dispatcher-singleton`

**Input**: A framework-claimed event batch, the registered sink set, the dispatcher's system-level `SecurityContext`,
per-sink timeout

**Output**: Per-event ack or retry returned to the Outbox framework; telemetry updated

**Steps**:
1. [ ] - `p1` - The `toolkit-db` Outbox processor claims the batch under a DB lease; the handler future is dropped at
   the cancel point (`lease_duration − ack_headroom`), so an expired holder can never overlap its successor - `inst-disp-claim`
2. [ ] - `p1` - **FOR EACH** event in the claimed batch - `inst-disp-each`
   1. [ ] - `p1` - API: `dispatch(ctx_system, event)` to all registered sinks concurrently, each bounded by the
      per-sink timeout - `inst-disp-fanout`
3. [ ] - `p1` - **IF** the registered sink set is empty - `inst-disp-nosink-if`
   1. [ ] - `p1` - **RETURN** ack unprocessed: events are dropped silently per the PRD §11 assumption, behind the
      bootstrap warning - `inst-disp-nosink`
4. [ ] - `p1` - **FOR EACH** `(event, sink)` outcome - `inst-disp-outcome`
   1. [ ] - `p1` - `Success`: record delivered for that sink - `inst-disp-ok`
   2. [ ] - `p1` - `Timeout` / `Transient`: increment `notification_dispatch_failures_total{sink_id, event_kind}` and
      mark the event retryable - `inst-disp-transient`
   3. [ ] - `p1` - `Permanent`: increment the failure counter and mark the event rejected - `inst-disp-perm`
5. [ ] - `p1` - **IF** any sink answered `Permanent`, or the event is retryable and `OutboxMessage.attempts` has
   reached the operator-configured maximum - `inst-disp-reject-if`
   1. [ ] - `p1` - **RETURN** `Reject(reason)`: the framework dead-letters the event; ToolKit itself never stops
      retrying, so the attempts guard is QE's explicit give-up per the `OutboxMessage.attempts` contract; increment
      `outbox_rejections_total` by `queue` on this path - `inst-disp-reject`
6. [ ] - `p1` - **IF** the event is retryable below the maximum - `inst-disp-retry-if`
   1. [ ] - `p1` - **RETURN** `Retry`: the framework re-delivers to **all** sinks later, duplicates permitted (sinks
      are idempotent on `event_id`) - `inst-disp-retry`
7. [ ] - `p1` - **RETURN** ack when every sink answered `Success`; no counter moves on this path
   (`outbox_rejections_total` increments only on `Reject`) - `inst-disp-ack`

### Threshold-Crossed Emission Semantics

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-threshold-emission`

**Input**: Pre/post consumed values of a successful counter mutation, the Quota's `notification_thresholds`, the
per-`(Quota, period)` highest-crossed marker. This feature owns the shared emission routine; the mutating call sites
that invoke it land with consumption-operations and lease-operations.

**Output**: Zero or one `threshold-crossed` outbox event for the mutation

**Steps**:
1. [ ] - `p1` - **IF** the operation outcome is `Denied` or a canonical error - `inst-thr-denied-if`
   1. [ ] - `p1` - Emit nothing: counters did not move, so no transition occurred - `inst-thr-none`
2. [ ] - `p1` - **IF** the mutation settles into a closing period during the settlement window (cross-period lease
   commit/release/rollback per ADR-0004) - `inst-thr-settle-if`
   1. [ ] - `p1` - Emit nothing: the settlement-window emit policy is silence; closing-period state rides the
      `period-rollover` payload alone - `inst-thr-settle-skip`
3. [ ] - `p1` - Compute the crossed set: thresholds `t` with `pre% < t ≤ post%` that are also strictly above the
   stored marker (the marker guards against re-emission after credits lower `consumed`) - `inst-thr-compute`
4. [ ] - `p1` - **IF** the crossed set is empty - `inst-thr-empty-if`
   1. [ ] - `p1` - Emit nothing - `inst-thr-skip`
5. [ ] - `p1` - Enqueue exactly one `threshold-crossed` event carrying `crossed_thresholds` ascending and `highest_crossed_threshold`, same-tx with the mutation - `inst-thr-emit`
6. [ ] - `p1` - DB: advance the stored marker to the highest crossed value; the marker resets at period rollover (I13, owned by consumption-operations) - `inst-thr-marker`
7. [ ] - `p1` - **RETURN** one event per upward transition, never per threshold and never on repeat readings - `inst-thr-return`

## 4. States (CDSL)

### Outbox Event State Machine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-state-outbox-event`

**States**: Enqueued, Delivered, DeadLettered

**Initial State**: Enqueued

**Transitions**:
1. [ ] - `p1` - **FROM** Enqueued **TO** Delivered **WHEN** the handler acks — every registered sink answered
   `Success` (or the sink set is empty per the zero-sink drop rule) - `inst-obst-delivered`
2. [ ] - `p1` - **FROM** Enqueued **TO** Enqueued **WHEN** the handler returns `Retry` on `Timeout`/`Transient`
   outcomes below the attempts maximum — re-delivery goes to all sinks; duplicates permitted - `inst-obst-retry`
3. [ ] - `p1` - **FROM** Enqueued **TO** DeadLettered **WHEN** the handler returns `Reject` — any `Permanent` sink
   outcome, or `OutboxMessage.attempts` at the configured maximum; operators inspect and replay via the framework
   `dead_letter_*` APIs - `inst-obst-dead`
4. [ ] - `p1` - **FROM** Delivered **TO** Delivered **WHEN** the framework vacuum stage reclaims the row (terminal;
   physical cleanup only) - `inst-obst-reclaim`

Dead-letter rows are retained per operator configuration for delivery-failure diagnostics (PRD §6.2 default: 7 days).

## 5. Definitions of Done

### Sink Plugin Contract

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-sink-contract`

The system **MUST** define `QuotaNotificationSinkV1` in the SDK crate — async, two methods (`id() -> &str`,
`dispatch(ctx: &SecurityContext, event: QuotaEvent) -> Result<(), DispatchError>`, invoked under the dispatcher's
system-level context) — with the documented obligation that sinks tolerate duplicate delivery of the same `event_id`,
and with the closed `DispatchError` enum
(`Timeout`, `Transient(String)`, `Permanent(String)`) and the `QuotaEvent` shape carrying the closed event-kind enum,
event-kind payloads, discriminators (`quota-changed.change_kind`, `policy-changed.change_kind` with rollback reported
as `updated`), `event_id`, `tenant_id`, target reference, `subject` when applicable, and the emission timestamp.

**Implements**:
- `cpt-cf-quota-enforcement-flow-sink-delivery`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: `QuotaNotificationSinkV1` (SDK trait)
- Entities: `NotificationOutboxEvent`

### Dispatcher as Outbox Leased Handler

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-dispatcher`

The system **MUST** implement the `NotificationDispatcher` as the sole caller of sink `dispatch`, registered as the
`toolkit-db` Outbox **leased handler** on the QE notification queue: the framework owns batch claiming, lease fencing
(handler future dropped at the cancel point), redelivery, and the dead-letter store; the handler owns concurrent
fan-out with a per-sink timeout, per-sink failure isolation, `Retry` on transient outcomes below the
operator-configured `OutboxMessage.attempts` maximum (duplicates permitted), `Reject` on any `Permanent` outcome or at
that maximum, and ack only when every sink succeeded — all under the dispatcher's system-level `SecurityContext`.

**Implements**:
- `cpt-cf-quota-enforcement-algo-dispatcher-singleton`
- `cpt-cf-quota-enforcement-state-outbox-event`

**Constraints**: `cpt-cf-quota-enforcement-constraint-security-context`,
`cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: `toolkit-db` Outbox builder/leased-handler registration and `dead_letter_*` APIs
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `NotificationOutboxEvent`

### Event Catalog Conformance

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-event-catalog`

The system **MUST** deliver all eight catalog kinds — `threshold-crossed`, `period-rollover`, `lease-auto-released`,
`lease-resolved-by-deactivation`, `quota-changed`, `quota-counter-adjusted`, `quota-rollback-applied`,
`policy-changed` — with the payload fields PRD §5.15 names per kind, and **MUST** implement the threshold
upward-transition semantics of this feature's emission process. Tenant-scope filtering is sink-side in P1.

**Implements**:
- `cpt-cf-quota-enforcement-algo-threshold-emission`
- `cpt-cf-quota-enforcement-flow-sink-delivery`

**Constraints**: `cpt-cf-quota-enforcement-constraint-bounded-cardinality`

**Touches**:
- API: `QuotaEvent` payload contract
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `NotificationOutboxEvent`

### Dispatch Telemetry

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-dispatch-telemetry`

The system **MUST** expose `notification_dispatch_failures_total` labelled by `sink_id` and `event_kind`,
`outbox_pending_rows` by `queue`, and `outbox_rejections_total` by `queue`; all three are enumerated in PRD §5.16
with those label dimensions in its permitted set. `outbox_rejections_total` is a monotonic counter that the handler
increments when it returns `Reject`; it is not sourced from the ToolKit `dead_letter_count` API, because that API
returns a current row count that decreases on replay, resolve, and cleanup, so it cannot back a counter.
`outbox_pending_rows` requires a pending-count API that `toolkit-db` does not expose today; adding it is a tracked
upstream prerequisite of this feature, and QE does not query the framework's tables directly.

**Implements**:
- `cpt-cf-quota-enforcement-algo-dispatcher-singleton`

**Constraints**: `cpt-cf-quota-enforcement-constraint-bounded-cardinality`

**Touches**:
- API: platform observability stack (`tracing` + `toolkit` `otel` feature)
- Entities: dispatch instruments per PRD §5.16 / DESIGN §4.1

## 6. Acceptance Criteria

- [ ] All eight event kinds reach every registered sink; each carries `event_id`, `event_kind`, `tenant_id`, its
  target reference, `subject` when applicable, and the emission timestamp
- [ ] A `Permanent` sink outcome maps to `Reject`: the event lands in the framework dead-letter store,
  `outbox_rejections_total` grows, and no quota write is blocked or slowed (best-effort verified under sustained
  sink failure)
- [ ] Transient failures retry until the operator-configured `OutboxMessage.attempts` maximum, then `Reject`
  dead-letters the event — ToolKit itself retries indefinitely, so the guard is verified as QE handler behavior
- [ ] A sink timeout affects only that sink within the invocation: other registered sinks receive the same event in
  the same fan-out; the event is then re-delivered to all sinks (duplicates tolerated per contract)
- [ ] Killing the dispatcher mid-batch loses no event: the lease expires, the framework drops the handler future at
  the cancel point, and another processor re-claims and re-delivers — sinks observe at-least-once delivery
- [ ] With two gateway replicas, batches are never processed concurrently by both: lease fencing guarantees an expired
  holder cannot ack after its successor claims
- [ ] Threshold routine unit test: inputs (marker unset, pre 30, post 85, thresholds `[50, 80, 100]`) yield exactly
  one event with `crossed_thresholds = [50, 80]` and `highest = 80`; inputs (marker 80, pre 85, post 90) yield none;
  a `Denied`/error outcome yields none — the end-to-end debit-driven verification is owned by consumption-operations
- [ ] The `policy-changed` payload contract admits `change_kind` only from the closed set `{created, updated,
  deleted}` — no `rolled_back` discriminator exists; the rule that `rollback_policy` emits `change_kind = "updated"`
  is verified end-to-end by the resolution-policy-engine feature
- [ ] Bootstrap with zero registered sinks surfaces the "no notification sinks registered" warning, the gear still
  serves writes, and the handler acks events unprocessed — dropped silently per the PRD §11 assumption

## 7. Additional Context (optional)

- **Rollout / rollback**: the dispatcher ships inside the gateway binary by default and follows its rolling update;
  the Outbox lease fences processing throughout. Rollback is binary redeploy; pending outbox rows are drained by
  whichever replica's processor claims them, and the event payload schema is additive within the gear major version.
- **Test layering**: threshold-set computation, retry classification, and per-sink bookkeeping get unit tests;
  singleton handoff, at-least-once redelivery, and sink-isolation behavior get integration tests with faulty sink
  doubles; the two-replica takeover criterion runs as a chaos test.
- **Non-applicable review domains**: UX/accessibility is not applicable — no user-facing surface. Data protection and
  compliance inherit the Platform Operational Data rules from PRD §6.2; dispatch records follow the 7-day diagnostic
  retention, with no additional feature-specific requirements.
