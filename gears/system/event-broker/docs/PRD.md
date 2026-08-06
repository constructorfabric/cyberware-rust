# PRD — Event Broker


<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
  - [1.5 Domain Model](#15-domain-model)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Module-Specific Environment Constraints](#31-module-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Topic & Event-Type Definition](#51-topic--event-type-definition)
  - [5.2 Producer Path (Ingest)](#52-producer-path-ingest)
  - [5.3 Consumer Path (Delivery)](#53-consumer-path-delivery)
  - [5.4 Consumer Group Registry](#54-consumer-group-registry)
  - [5.5 Storage Backend Plugin System](#55-storage-backend-plugin-system)
  - [5.6 Authorization](#56-authorization)
  - [5.7 Error Codes](#57-error-codes)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Module-Specific NFRs](#61-module-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
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

<!--
=============================================================================
PRODUCT REQUIREMENTS DOCUMENT (PRD)
=============================================================================
PURPOSE: Define WHAT the system must do and WHY — business requirements,
functional capabilities, and quality attributes.

SCOPE:
  ✓ Business goals and success criteria
  ✓ Actors (users, systems) that interact with this module
  ✓ Functional requirements (WHAT, not HOW)
  ✓ Non-functional requirements (quality attributes, SLOs)
  ✓ Scope boundaries (in/out of scope)
  ✓ Assumptions, dependencies, risks

NOT IN THIS DOCUMENT (see other templates):
  ✗ Stakeholder needs (managed at project/task level by steering committee)
  ✗ Technical architecture, design decisions → DESIGN.md
  ✗ Why a specific technical approach was chosen → ADR/
  ✗ Detailed implementation flows, algorithms → features/

STANDARDS ALIGNMENT:
  - IEEE 830 / ISO/IEC/IEEE 29148:2018 (requirements specification)
  - IEEE 1233 (system requirements)
  - ISO/IEC 15288 / 12207 (requirements definition)

REQUIREMENT LANGUAGE:
  - Use "MUST" or "SHALL" for mandatory requirements (implicit default)
  - Do not use "SHOULD" or "MAY" — use priority p2/p3 instead
  - Be specific and clear; no fluff, bloat, duplication, or emoji
=============================================================================
-->
## 1. Overview

### 1.1 Purpose

The Event Broker is a tenant-scoped event streaming primitive for Gears modules. It enables decoupled inter-module communication, real-time data streaming to subscribers, and reliable replay of historical events. Producers publish typed events to topics; consumers subscribe via streaming and receive events filtered by type, subject, and dynamic filter expressions (pluggable filter engines; the specific expression language is not fixed at the requirements level). On top of this primitive, modules build higher-level patterns — **notification fan-out and audit streams** — without each re-implementing fan-out, ordering, or replay.

The broker is a platform-native module — it ships in-process with the Gears framework, integrates with the existing transactional outbox (`toolkit-db`), and consumes platform coordination primitives (`cluster` system module). It does NOT replace external messaging systems for inter-region streaming, but it does provide a self-contained Kafka-style event log for intra-platform communication without requiring an external broker (Kafka, NATS, etc.).

### 1.2 Background / Problem Statement

Gears modules currently couple either through direct SDK calls (synchronous, no replay, point-to-point) or through `toolkit-db`'s transactional outbox (one-shot delivery, no fan-out, no consumer groups, no historical query). Neither pattern fits the use cases that need:

- **Multi-consumer fan-out**: many subscribers need the same event (audit, billing, analytics, search index, notifications).
- **Replayable history**: a new subscriber needs to backfill from a known offset, not just receive future events.
- **Decoupled lifecycle**: producers and consumers deploy independently and may not be running at the same time.
- **At-least-once delivery with idempotent processing**: the standard event-streaming contract.

Today a module needing these properties must either bring up an external broker (Kafka/NATS), depend on a peer module that exposes events directly (tight coupling), or build a per-module fan-out mechanism. The Event Broker closes this gap as a platform primitive.

### 1.3 Goals (Business Outcomes)

- Provide a **single, opinionated event-streaming primitive** for every Gears module — eliminate per-module fan-out implementations and external-broker dependencies for intra-platform use cases.
- Standardize on **GTS-typed events** so producer / consumer contracts are explicit, validated, and discoverable via `types_registry`.
- Preserve **at-least-once delivery with idempotent producers and consumers** — the standard contract platform modules can rely on.
- Support **multiple deployment topologies** (standalone single-process, cluster with multiple ingest/delivery shards) without changing module-level code — wire format and SDK contract are identical.
- Allow **third-party storage backends** (Kafka, S3, custom) to plug in without modifying broker core, so deployment-specific scale or compliance needs can be met.

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Topic | A named, partitioned event stream. A topic is an instance of `gts.cf.core.events.topic.v1~` (for example `gts.cf.core.events.topic.v1~vendor.foo.v1`) carrying the stream's own data - identifier, description, and how long its events are kept. Partition count and backend binding are broker configuration. Topic is the unit of subscription and the scope of partition / offset semantics. Topics are platform-scoped (globally unique by GTS). See [DESIGN.md §3.1 Topic Schema](DESIGN.md). |
| Partition | An independent ordered log within a topic. Partitioning serves two requirements — per-key event ordering and horizontal scale — rather than being a requirement itself. Per-partition order is total; cross-partition order is unspecified. Which member of an event decides its partition is declared by the event's *type*, as a JSON Pointer defaulting to the event's tenant - so routing is one decision per type, shared by every publisher of it, and a publish body carries neither a partition nor a partition key. Partition count is broker configuration, keyed by topic identifier, defaulting to 8 for any topic no entry names. See [ADR-0002](ADR/0002-partition-selection.md). |
| Event | An immutable record in a `(topic, partition)` log, GTS-typed via its `type` - a GTS **type** identifier, since a concrete event type is a derived type schema. Carries `subject` + `subject_type` to identify the entity it concerns; its topic and its partition are both resolved from its type rather than supplied with it. See [DESIGN.md §3.1 Event Schema](DESIGN.md) and [ADR-0003 Event Schema](ADR/0003-event-schema.md). |
| Event Type | A derived GTS type schema describing one kind of event. It narrows the base event schema's `data` member into the payload contract that validates `event.data` at ingest, and declares the metadata governing the type - owning topic, allowed subject types - in its `x-gts-traits`. See [DESIGN.md §3.1 Event Type](DESIGN.md). |
| Subject Type | The GTS-typed kind of entity an event concerns (e.g., user, account, invoice). Enforced on two dimensions: authorization (per-principal) and schema (`event_type.allowed_subject_types`). |
| Producer | A client that publishes events. Three modes: chained (producer-id + previous + sequence chain), monotonic (producer-id + sequence), stateless (no producer-id). |
| Consumer Group | A logical set of cooperating consumer instances sharing partition assignments and a single committed offset per partition. Persistent broker resource, GTS-typed, with two creation paths (anonymous via `POST /v1/consumer-groups`, named via `types_registry`). |
| Subscription | An ephemeral session a consumer instance opens against a consumer group via `POST /v1/subscriptions`. Holds the partition assignment, declared `interests[]`, compiled filter handles, and streaming session state. Identified by `subscription_id`; lives in cache; expires on `session_timeout`. See [DESIGN.md §3.1 Subscription Schema](DESIGN.md). |
| Interest | A topic-anchored, typed-filter selection declared at JOIN per [ADR-0005](ADR/0005-subscription-filter-typing.md). One subscription carries 1–64 interests. Each interest has required `topic`, `tenant_id`, `types[]` plus an optional `filter { engine, expression }` object. Multiple interests OR together. |
| FilterEngine | Plugin trait (`compile()` + `eval()`) for evaluating filter expressions over events. GTS-typed; base type `gts.cf.core.events.filter.v1~`; v1 built-in CEL engine. Resolved at JOIN via `ClientHub`. |
| FilterContext | Per-engine declaration of which event fields are visible to filter expressions. CEL engine v1 exposes the read-side event (`gts.cf.core.events.event.v1~.schema.json` with `readOnly` fields populated; `writeOnly` `meta` stripped). |
| Offset | Backend-assigned, monotonic-per-`(topic, partition)` ordering key. Consumer-visible; the only sequence consumers paginate by. |
| Storage Backend | Pluggable persistence layer for events. Discovered via GTS instance registration and resolved via ClientHub. Built-in implementations: memory, postgres. Backend enforces retention, compaction, and offset assignment. |
| Cluster Module | The platform-level `modules/system/cluster` system module providing KV-with-TTL, leader election, and distributed locks. Consumed by the broker for coordination. |
| GTS | Global Type System — the platform's schema and instance registration system used for type identification, validation, and pattern matching in authorization. |

### 1.5 Domain Model

Before the functional requirements, this section establishes the conceptual entities the broker deals in and how they relate. It is the requirements-level view; the schema-level model (fields, persistence, GTS base types) lives in [DESIGN.md §3.1 Domain Model](DESIGN.md).

1. **Topic** — A named, platform-scoped event stream identified by a GTS topic identifier and the unit of subscription. It defines the scope within which event ordering and offsets are meaningful, and declares how long its events are kept. **Partition count** and the **backend binding** are broker configuration rather than topic data. Retention, compaction/consolidation, segment sizing and similar are **enforced by the topic's bound storage backend**; the broker performs none of them itself.

2. **Event Type** — A versioned contract, identified by a GTS identifier and bound to exactly one parent **topic**, that constrains the events published under it. It declares the set of **allowed subject types** and a **payload schema** (`data_schema`) against which each event's payload is validated at ingest. Event types are **immutable** in v1: changing the contract requires a new GTS identifier.

3. **Event** — An **immutable**, append-only record of something that occurred, classified by its **event type**. Each event carries a client-supplied identifier, the **subject** and **subject type** identifying the entity it concerns, its owning **tenant**, an occurrence **timestamp**, and a typed **payload** whose shape is governed by the event type's schema. An event's **partition is derived by the broker** (by default from the tenant) and is never set by the producer. Once accepted, an event is never modified, re-ordered, or re-numbered.

4. **Partition** — An independent, totally-ordered log within a topic. Ordering is **total within** a single partition and **unspecified across** partitions. The partition count is broker configuration. Partitioning determines the granularity of ordering, of parallel consumption, and of offset tracking.

5. **Subscription** — An **ephemeral** session opened by a single consumer instance against a consumer group. It holds the **partition assignment** granted to that instance and the consumer's declared **interests** — the topics, event types, and optional filter expression it wishes to receive. A subscription exists only for the lifetime of the consuming session and expires on timeout; it never outlives the consumer process.

6. **Consumer Group** — A named, **persistent** grouping of cooperating consumer instances that together consume a set of topics. The group is the unit across which **partitions are distributed** (a partition is processed by at most one member at a time) and against which delivery progress is committed. Membership is dynamic; the group's committed position survives individual member churn.

7. **Offset / Cursor** — The **offset** is a backend-assigned, monotonic position within a `(topic, partition)` log; it is the only ordering coordinate consumers observe. The **cursor** is the group-scoped ephemeral session position set by SEEK. The cursor advances **forward-only** during streaming and may be repositioned explicitly for replay via SEEK (`POST /v1/subscriptions/{id}:seek`).

8. **Storage Backend** — The pluggable persistence layer bound to a topic. It durably stores events, **assigns their offsets**, and **enforces retention, compaction/consolidation, and deletion** - retention as the topic declares it, the rest as its own configuration directs. Built-in backends are provided (in-memory for dev/test, PostgreSQL for production); third-party backends register via GTS without modifying broker core.

**Relationships**:

```
Topic 1 ──*  Event Type        (a topic defines its event types)
Topic 1 ──*  Partition         (fixed count, set at creation)
Event Type 1 ──*  Event        (an event is an instance of one type)
Partition 1 ──*  Event         (each event lands in exactly one partition)
Topic *  ──1  Storage Backend  (a topic is bound to one backend; the backend enforces retention/consolidation)
Consumer Group 1 ──*  Subscription   (members)
Consumer Group 1 ──*  Cursor         (one committed cursor per assigned partition)
Subscription *  ──*  Partition       (a subscription is assigned partitions to consume)
```

**Invariants**: events are immutable and append-only; an event's payload shape is governed by its event type and therefore varies across types; ordering guarantees hold **per partition only**; a tenant's events share a partition by default; retention and consolidation are enforced by the **storage backend**, not the broker.

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level by steering committee. Document **actors** (users, systems) that interact with this module.

### 2.1 Human Actors

#### Platform Operator

**ID**: `cpt-cf-evbk-actor-platform-operator`

- **Role**: Configures broker deployment topology (standalone vs cluster), partition count, bound storage backend instances per topic, and operational policies (stream connection caps, group caps, retention via backend config).
- **Needs**: Operate broker as a managed platform service; observe per-topic / per-group lag; scale delivery shards horizontally; rebind topics to different backend instances on infrastructure changes.

#### Module Developer (Producer Side)

**ID**: `cpt-cf-evbk-actor-module-dev-producer`

- **Role**: Authors a Gears module that publishes events. Registers topic and event-type schemas in `types_registry` and/or uses the producer SDK to enqueue events transactionally with business writes. Registration (`define`) and publishing (`produce`) are independently grantable authorization actions (see §5.6) — the module that registers a type need not be the module that publishes it.
- **Needs**: Transactional publish (events durable iff business commit succeeds); chain-of-custody dedup so retries don't double-publish; explicit error semantics on chain breaks; minimal coupling to broker internals.

#### Module Developer (Consumer Side)

**ID**: `cpt-cf-evbk-actor-module-dev-consumer`

- **Role**: Authors a Gears module that consumes events. Joins a consumer group, declares per-member topic / filter set, polls and acknowledges events; handles topology changes and re-JOIN semantics.
- **Needs**: At-least-once delivery; per-member filters (no canonical group filter to fight); a clear recovery contract distinguishing shard shutdown from subscription loss, with re-JOIN semantics; ability to seek for replay.

### 2.2 System Actors

#### `toolkit-db` (Outbox)

**ID**: `cpt-cf-evbk-actor-modkit-db`

- **Role**: Provides the transactional outbox the producer SDK and ingest service both build on. Owns the four-stage outbox pipeline (enqueue → sequencer → processor → vacuum). The broker contributes nothing to the outbox library; it just uses it.

#### Cluster System Module

**ID**: `cpt-cf-evbk-actor-cluster`

- **Role**: Provides `ClusterCacheV1` (KV with TTL, CAS, watch), `LeaderElectionV1`, and `DistributedLockV1`. The broker uses these primitives for subscription / group state caching, group-rebalance locks, and Reaper singleton election. Shard discovery / liveness is **not** a cluster-gear capability — it resolves through `DirectoryService` (ADR-0009 instance-addressable discovery).

#### `authz-resolver`

**ID**: `cpt-cf-evbk-actor-authz-resolver`

- **Role**: Provides the `PolicyEnforcer` PEP for per-(resource, action) authorization. The broker delegates all authorization decisions to this framework — no broker-local authz rules. AccessScope constraints from PEP are AND-merged into broker queries.

#### `tenant-resolver`

**ID**: `cpt-cf-evbk-actor-tenant-resolver`

- **Role**: Resolves tenant identities and hierarchy. Provides `ROOT_TENANT_ID` for inter-service traffic. The broker reads tenant context from `SecurityContext` set by the platform's authentication layer.

#### `types-registry`

**ID**: `cpt-cf-evbk-actor-types-registry`

- **Role**: Stores GTS schema and instance registrations. The broker uses it to validate topic / event-type / consumer-group GTS identifiers; to resolve the event-type `data_schema` against which event payloads are validated at ingest; reads named consumer-group instances at startup to upsert `evbk_consumer_group` rows; resolves storage backend type instances for ClientHub-based backend resolution.

#### Storage Backend Plugin

**ID**: `cpt-cf-evbk-actor-storage-backend`

- **Role**: Persists events. Implements the `StorageBackend` async trait (`persist`, `query`, `truncate`, `segments`). Owns retention, compaction, offset assignment. Built-in plugins: memory, postgres. Third-party plugins (Kafka, S3) plug in via GTS type registration without modifying broker core.

## 3. Operational Concept & Environment

> **Note**: Project-wide runtime, OS, architecture, lifecycle policy, and integration patterns defined in root PRD. Document only module-specific deviations here.

### 3.1 Module-Specific Environment Constraints

The Event Broker follows standard Gears module conventions. Module-specific deployment notes:

- **Two deployment topologies** are first-class: **Standalone** (single process; in-process `cluster` provider; one ingest + one delivery in-process) and **Cluster** (separate ingest / delivery / dispatcher processes, scaled independently, coordinating via the platform `cluster` module backed by Redis / etcd / NATS).
- **Per-module DB schema invariant**: in standalone mode, the producer module's outbox and the broker module's ingest outbox live in distinct DB schemas (one per Gears module), so no table-name collision arises.
- **Two streaming-delivery deployment patterns** for ingest in cluster mode: **hetero** (every ingest serves every topic; load-balance across ingest set; default) and **sharded** (specific topic patterns pinned to specific ingest instances; opt-in for noisy-neighbor isolation or hardware affinity).

## 4. Scope

### 4.1 In Scope

- REST API for produce (single + batch), consume (stream, ack, seek, leave), producer lifecycle, consumer-group lifecycle, topic introspection.
- Three producer modes (chained / monotonic / stateless) with broker-side chain-of-custody dedup at ingest.
- Long-poll consumption with per-partition append-only in-memory cache and iterator-driven notification fan-out.
- GTS-typed topics, event types, subject types, consumer groups; full GTS pattern wildcards in authorization.
- Storage backend plugin contract with built-in memory and postgres implementations; vendor-extensible via GTS instance registration.
- Per-(resource, action) authorization via the `authz-resolver` framework (4 resource types × 3 actions: `produce`, `consume`, `define`).
- Persistent consumer-group registry (`evbk_consumer_group`) with two exclusive provisioning paths (anonymous via REST, named via `types_registry`).
- Per-event size limit (64 KiB combined headers + payload); per-batch limit (100 events / 1 MiB).
- Cluster-mode failover with `DirectoryService` as source of truth for delivery-shard liveness.
- Standalone and cluster deployment topologies; hetero and sharded ingest routing modes.
- RFC 9457 Problem Details for all error responses with stable GTS type identifiers.

### 4.2 Out of Scope

- Consumer-facing dead-letter queues — the broker provides none. A consumer that needs one creates its own dedicated dead-letter **topic** (single event type, `object`-typed payload) and republishes events it cannot process. (Distinct from the ingest-side *outbox dead-letter* that records terminal `backend.persist` failures for operators — see DESIGN §3.7.)
- Webhooks / outbound HTTP push delivery — the added load, authorization, and retry semantics belong in a separate component adjacent to the broker, not in the broker itself.
- Event-type schema evolution (forward / backward compatibility, inter-version casting). v1 treats event types as immutable; schema changes require a new GTS event-type identifier.
- Quotas and rate limiting. Limits **must** exist before broad production exposure — scoped **per topic, per producer, and per consumer** (produce rate, poll rate, active-subscription and resource-creation caps, storage quotas) — but the full scheme needs dedicated design and is deferred to the post-MVP **Quotas & Rate Limiting** feature (see §12 Risks). Interim per-tenant safety-floor caps on resource-creation endpoints are tracked now (see §6.2, `cpt-cf-evbk-nfr-tenant-rate-caps`).
- Topic soft-delete with grace-period serving.
- Backend-failure backpressure (ingest auto-marking topics as failed and refusing produces).
- DB-restore resync tooling (CLI to realign producer / consumer state after broker DB restore).
- Finalized per-language SDK designs (concrete Rust/Go/etc. trait shapes, error enums, retry and reconnection policies). This PRD/DESIGN specifies the wire contract that any SDK must honour; each language SDK's internal design is produced during implementation, not here.
- Broker admin / debug endpoints (replay from timestamp, browse last N events on a partition).
- Testing infrastructure design (cluster-mode integration test harness, two-outbox pipeline test harness, time-scaled test mode) — separate design.
- Cooperative cross-tenant consumption on anonymous groups (the future `shared_with` mechanism).

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting 90%+ code coverage unless otherwise specified. Document verification method only for non-test approaches (analysis, inspection, demonstration).

### 5.1 Topic & Event-Type Definition

#### Topic Registration

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-topic-registration`

The system MUST support topic registration through the Type Registry: topics are authored declaratively (e.g. in YAML) and owned by `types_registry` as instances of `gts.cf.core.events.topic.v1~`; the broker consumes the registered instances at startup (it does not parse declarative config itself). A registered topic MUST have a globally-unique GTS instance identifier (`gts.cf.core.events.topic.v1~<vendor>.<package>.<namespace>.<name>.v1`) and MUST declare a description and MAY declare a retention. A topic carries only the stream's own data; the partition count and the backend binding are broker configuration, because they describe how a deployment serves the stream rather than what the stream is.

- **Rationale**: Topics are platform-scoped contracts; registration is identity-and-ownership state that must be unambiguous across deployments.
- **Actors**: `cpt-cf-evbk-actor-platform-operator`, `cpt-cf-evbk-actor-module-dev-producer`

#### Event-Type Registration

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-event-type-registration`

The system MUST support event-type registration via `types_registry`. Each event type MUST have a GTS identifier, a parent topic, an `allowed_subject_types` list (GTS patterns; wildcards permitted), and a JSON Schema for the `data` payload. Event types are immutable in v1 — schema changes require a new GTS identifier. Where a publish or registration references a concrete type, the broker MUST require a fully-qualified GTS identifier (carrying the version) and reject an under-qualified reference; version-granularity and resolution semantics (major/minor) are owned by GTS / the Type Registry, not the broker.

- **Rationale**: Event-type schemas are part of the contract producers and consumers depend on; immutability ensures consumers don't break under producer changes.
- **Actors**: `cpt-cf-evbk-actor-module-dev-producer`, `cpt-cf-evbk-actor-types-registry`

#### Topic Introspection

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-topic-introspection`

The system MUST expose read-only REST endpoints for listing topics, listing event types, retrieving a single topic's metadata, and retrieving a single event type's schema. Read access is gated by the `topic:consume` PEP check.

- **Rationale**: Producers and SDKs may need topic metadata for local broker-partition hints or batching, consumers need event-type schemas to validate, and tooling needs discovery.
- **Actors**: `cpt-cf-evbk-actor-module-dev-producer`, `cpt-cf-evbk-actor-module-dev-consumer`

### 5.2 Producer Path (Ingest)

#### Single-Event Publish

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-publish-single`

The system MUST accept single-event publish requests. The event MUST carry `id` (UUID, client-provided), `type`, `subject`, `subject_type`, `tenant_id`, and `data`; the owning topic is resolved from the event type, and the partition is broker-derived, not client-set. Publish MUST be **accepted asynchronously and durably** — the broker acknowledges once the event is durably recorded in the ingest outbox, with persistence to the storage backend completing asynchronously under an at-least-once guarantee. A synchronous mode MUST be available for callers needing read-after-write, acknowledging only after backend persistence completes. Wire endpoint, status codes, and headers are defined in DESIGN §3.3.

- **Rationale**: Standard event-streaming publish primitive with explicit at-least-once semantics.
- **Actors**: `cpt-cf-evbk-actor-module-dev-producer`

#### Batch Publish

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-publish-batch`

The system MUST accept batch publish via `POST /v1/events:batch`. All events in a batch MUST resolve to the same `(topic, partition)` — i.e. share the same topic and the same partitioning input (default: same `tenant`). Hard limits: 100 events per batch, 1 MiB total payload, 64 KiB per event. The batch is atomic — if any event fails dedup validation, the entire batch is rejected.

- **Rationale**: High-throughput producers benefit from batching; same-(topic, partition) requirement keeps batching aligned with the dispatcher's routing model.
- **Actors**: `cpt-cf-evbk-actor-module-dev-producer`

#### Producer Modes

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-producer-modes`

The system MUST recognize three producer modes inferred from the fields set on each event: **chained** (`producer_id` + `previous` + `sequence`), **monotonic** (`producer_id` + `sequence` only), **stateless** (none). Chained mode MUST verify `event.previous == state.last_sequence` AND `event.sequence > state.last_sequence` against `evbk_producer_state`; mismatched chains MUST surface as `412 SequenceViolation`. Monotonic mode MUST verify only `event.sequence > state.last_sequence`. Stateless mode MUST do no broker-side dedup. On `412 SequenceViolation` there are two cases. **(1) Transient async retry / duplicate** — resolved automatically with no human involvement: a duplicate is treated as success, and a benign reorder is re-driven from the broker's cursor. **(2) Genuine divergence** — e.g. the producer's local DB was restored from backup and its sequence regressed — which cannot be reconciled silently and MUST surface for **operator acknowledgement**; the operator reconciles against `GET /v1/producers/{producer_id}/cursors` (resume from `last_sequence + 1`) or rotates to a fresh `producer_id` to start a new chain.

- **Rationale**: Different producer profiles need different dedup guarantees; chain-of-custody is the broker-side primitive that supports all three without an event.id index.
- **Actors**: `cpt-cf-evbk-actor-module-dev-producer`

#### Producer Registration

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-producer-registration`

The system MUST expose `POST /v1/producers` returning a server-issued `id` (UUID) bound to the calling principal. Subsequent `POST /v1/events` requests with this `Producer-Id` header MUST be accepted only when the call's principal matches the registered owner; mismatch MUST surface as `403 Forbidden`.

- **Rationale**: Prevents unauthenticated `Producer-Id` claiming and same-tenant ordering chaos.
- **Actors**: `cpt-cf-evbk-actor-module-dev-producer`

#### Producer Cursor Recovery

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-producer-cursors`

The system MUST expose `GET /v1/producers/{producer_id}/cursors` returning the broker's known `last_sequence` per `(topic, partition)` for the registered producer. Producers MUST use this for desync recovery (DB restore, restart without persistent state, suspected divergence).

- **Rationale**: Recovery from chain-state divergence requires producer to read broker's authoritative view.
- **Actors**: `cpt-cf-evbk-actor-module-dev-producer`

### 5.3 Consumer Path (Delivery)

#### Subscription Creation (JOIN)

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-subscription-join`

The system MUST accept `POST /v1/subscriptions` (JOIN) with body containing `consumer_group` (GTS identifier, required), `client_agent` (RFC 9110 User-Agent grammar, required, per ADR-0004), `interests[]` (≥1, ≤64 entries), and `session_timeout`. Each entry in `interests[]` is a topic-anchored typed-filter selection per [ADR/0005-subscription-filter-typing.md](ADR/0005-subscription-filter-typing.md), with three required fields (`topic`, `tenant_id`, `types`) and an optional `filter { engine, expression }` object (both `engine` and `expression` required when `filter` is present). The system MUST validate the consumer group exists; for each interest, validate topic existence + topic-level `consume` authz + per-tenant authz + type-pattern syntax + topic-scoped pattern resolution (per-name latest + minor-version-omitted) + per-type authz + (if `filter` supplied) engine resolution + expression compilation. Failed JOIN MUST leave no broker state. On success, the broker computes the topic set as the union of `interest.topic` values, runs the rebalance algorithm, and returns `subscription_id` + `assigned[]` (topic, partition pairs) + `topology_version`. Subject to the per-tenant JOIN rate cap (`cpt-cf-evbk-nfr-tenant-rate-caps`).

- **Rationale**: The JOIN is the contract entry point for consumers; authorization, group identity, partition assignment, and filter-engine compilation all converge here. The interests model gives unambiguous topic-anchored declarations and the optional `filter` object allows the common "match topic+tenant+types" case to omit expressions entirely.
- **Actors**: `cpt-cf-evbk-actor-module-dev-consumer`

#### Typed Filter Expressions

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-filter-expression`

The system MUST support typed filter expressions as an optional `filter { engine, expression }` object on each interest, where `filter.engine` is a full GTS identifier extending the base type `gts.cf.core.events.filter.v1~` and `filter.expression` is the engine-specific source string. The v1 deliverable MUST include a built-in CEL engine (`gts.cf.core.events.filter.v1~cf.core.expression.cel.v1`). Additional engines (starlark, rego, vendor-custom) MUST be pluggable via the same GTS-typed registry pattern used for storage backends, without breaking the wire. The engine trait, filter context, and event-field visibility are defined in DESIGN / ADR-0005.

- **Rationale**: Typed filters open the door to additional expression languages without breaking the wire; GTS-typed engines are consistent with how every other plugin in the platform is identified.
- **Actors**: `cpt-cf-evbk-actor-module-dev-consumer`

#### Long-Poll Consumption

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-streaming-delivery`

The system MUST serve `GET /v1/events:stream?subscription_id={uuid}` as a long-lived `multipart/mixed` stream that emits one event per multipart part. The broker MUST emit `heartbeat` frames at a configurable cadence (default 5 s) on idle subscriptions. The response MUST include `topology` frames whenever assignment / topology version changes mid-stream. The stream MUST use a per-`(topic, partition)` in-memory append-only cache + iterator pattern to avoid per-group backend re-queries on event publish.

- **Rationale**: Streaming `multipart/mixed` is the only consumption surface in MVP for server-to-server consumers; per-partition cache + iterators is the design center for fan-out efficiency.
- **Actors**: `cpt-cf-evbk-actor-module-dev-consumer`

#### Seek (Cursor Advance)

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-ack-seek`

The system MUST support `POST /v1/subscriptions/{id}:seek` (advance cursor via SEEK, sets `cursor.offset` per `(topic, partition)`; forward-only during streaming via `MAX(current, requested)`; unrestricted before streaming opens). Cursor state MUST be group-scoped (shared across all subscriptions in the group), held in the cluster cache (no SQL `evbk_cursor` table).

- **Rationale**: Replay and cursor advance require seek; cursor's lifetime should match the group's, not any specific subscription.
- **Actors**: `cpt-cf-evbk-actor-module-dev-consumer`

#### Subscription Termination Recovery

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-subscription-recovery`

The system MUST distinguish graceful delivery-shard shutdown (in-flight streams drained) from ungraceful subscription loss, signalling each distinctly to the consumer. The SDK contract MUST require re-JOIN with the original `consumer_group` / `topics` / filters on either signal — the group cursor preserves position across re-JOIN when the cache survives. The specific status codes are defined in DESIGN §3.3.

- **Rationale**: Shard ownership migration is a routine operational event; clean, normative recovery semantics keep consumer code simple.
- **Actors**: `cpt-cf-evbk-actor-module-dev-consumer`, `cpt-cf-evbk-actor-cluster`

### 5.4 Consumer Group Registry

#### Anonymous Group Creation

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-anonymous-group-create`

The system MUST accept `POST /v1/consumer-groups` with a UUID-backed GTS identifier (`gts.cf.core.events.consumer_group.v1~{uuid}`). The row's `tenant_id` and `owner_principal` MUST come from `SecurityContext` and be non-overridable. Named (vendor-namespaced) identifiers MUST be rejected with `400 NamedGroupRequiresRegistry`.

- **Rationale**: Anonymous groups are caller-driven, tenant-private; this endpoint is the only path. Named groups have a separate, authority-controlled path.
- **Actors**: `cpt-cf-evbk-actor-module-dev-consumer`

#### Named Group Provisioning

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-named-group-provisioning`

The system MUST read `types_registry` at startup for instances of `gts.cf.core.events.consumer_group.v1~` and upsert each into `evbk_consumer_group` with `kind='named'` and `owner_principal` set to the registering module's principal. The upsert MUST be idempotent — re-running startup MUST NOT change ownership of existing named groups.

- **Rationale**: Named groups are platform-level resources; their lifecycle is tied to module registration, not REST.
- **Actors**: `cpt-cf-evbk-actor-platform-operator`, `cpt-cf-evbk-actor-types-registry`

#### Group JOIN Authorization

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-group-join-authz`

The system MUST enforce two distinct authorization paths at JOIN: **anonymous groups** require caller's `tenant_id` to equal the row's `owner_tenant_id` (`403 ConsumerGroupNotOwned` otherwise); **named groups** require an explicit `:consume` PEP grant on the concrete GTS instance via `authz-resolver`.

- **Rationale**: Anonymous groups are tenant-private by construction; named groups are the legitimate cross-tenant cooperative path gated by explicit grants.
- **Actors**: `cpt-cf-evbk-actor-authz-resolver`

### 5.5 Storage Backend Plugin System

#### Backend Trait Contract

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-backend-trait`

The system MUST define a `StorageBackend` async trait with four methods — `persist`, `query`, `truncate`, and `segments`. `persist` MUST NOT return offsets inline — the backend assigns offsets natively and consumers learn them via `query`. Method signatures, arguments, and error types are defined in DESIGN §3.2.

- **Rationale**: Minimal trait surface — backend owns storage and offset assignment; broker owns dedup, ordering (via outbox), and notification.
- **Actors**: `cpt-cf-evbk-actor-storage-backend`

#### Built-in Backends

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-builtin-backends`

The system MUST ship two built-in storage backend instances: `gts.cf.core.events.backend.v1~cf.core.backend.memory.v1` (in-memory, dev/test only, no persistence across restarts) and `gts.cf.core.events.backend.v1~cf.core.backend.postgres.v1` (SeaORM-based, multi-SQL: PostgreSQL primary, also MySQL / SQLite, production default).

- **Rationale**: Memory backend supports development and testing; postgres backend is the production default for self-contained deployments.
- **Actors**: `cpt-cf-evbk-actor-platform-operator`

#### Third-Party Backend Extension

- [ ] `p2` - **ID**: `cpt-cf-evbk-fr-third-party-backends`

The system MUST allow third-party storage backend plugins (e.g., Kafka, S3, custom) to register via GTS instance registration without modifying broker core. The plugin MUST implement the `StorageBackend` trait and its GTS type MUST extend `gts.cf.core.events.backend.v1~`. ClientHub MUST resolve the bound instance per-topic at runtime.

- **Rationale**: Deployment-specific scale or compliance needs (Kafka for cross-region, S3 for cold archival) shouldn't require forking the broker.
- **Actors**: `cpt-cf-evbk-actor-platform-operator`

### 5.6 Authorization

#### Per-Resource Authorization

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-authorization`

The system MUST delegate all authorization decisions to the `authz-resolver` framework via `PolicyEnforcer::access_scope_with(...)`. The broker MUST define four resource types — `gts.cf.core.events.topic.v1~`, `gts.cf.core.events.event.v1~`, `gts.cf.core.events.subject_type.v1~`, `gts.cf.core.events.consumer_group.v1~` — each with three actions (`produce`, `consume`, `define`). Permission grammar MUST be `<gts_pattern>:<action>` with full GTS wildcard support.

- **Rationale**: Standardized authorization via the platform PEP; per-resource granularity prevents the coarse-permission failure mode where any service with `produce` could write to any topic.
- **Actors**: `cpt-cf-evbk-actor-authz-resolver`

#### Subject-Type Dual Enforcement

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-subject-type-dual-enforcement`

The system MUST enforce subject-type validity on two independent dimensions: **authorization** (`subject_type:produce` / `:consume` PEP check at runtime) and **schema** (publish-time validation that `event.subject_type` is in the event-type's `allowed_subject_types`, which MAY contain GTS wildcards).

- **Rationale**: The two checks cover different concerns — capability vs correctness — and cannot substitute for each other.
- **Actors**: `cpt-cf-evbk-actor-authz-resolver`

### 5.7 Error Codes

#### RFC 9457 Problem Details

- [ ] `p1` - **ID**: `cpt-cf-evbk-fr-rfc9457-errors`

All error responses MUST follow RFC 9457 Problem Details (`application/problem+json`), reusing the canonical error categories produced by `toolkit-canonical-errors` — Event Broker MUST NOT define its own wire format or mint a custom `Problem.type`. The `type` MUST be the canonical category URI `gts://gts.cf.core.errors.err.v1~cf.core.err.<category>.v1~`. Each error MUST carry `type`, `title`, `status`, `detail`, and `context`. `instance` and `trace_id` are optional, boundary-injected members set by the canonical-errors middleware (absent otherwise), consistent with RFC 9457 and `toolkit-canonical-errors`. Domain-specific data (including `retry_after_seconds`) MUST live in the per-category `context` object; retry timing for 503 responses is additionally surfaced via the standard HTTP `Retry-After` header, which the canonical-errors layer owns (not Event Broker).

- **Rationale**: Consistent, machine-readable error format aligned with the platform's error model.
- **Actors**: All

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs (performance, security, reliability, scalability) defined in root PRD and [guidelines/](../../../../guidelines/). Document only module-specific NFRs here: **exclusions** from defaults or **standalone** requirements.
>
> **Testing strategy**: NFRs verified via automated benchmarks, security scans, and monitoring unless otherwise specified.

### 6.1 Module-Specific NFRs

#### Per-Event Size Limit

- [ ] `p1` - **ID**: `cpt-cf-evbk-nfr-event-size`

Per-event hard size limit: 64 KiB combined headers + payload. Enforced at ingest before validation. Larger events MUST be split or referenced (referenced-payload mechanism is post-MVP).

#### Long-Poll Timeout

- [ ] `p1` - **ID**: `cpt-cf-evbk-nfr-long-poll-timeout`

Long-poll maximum timeout: 30 seconds. Consumers behind aggressive corporate proxies SHOULD request a lower timeout (e.g., 20 seconds) to keep responses well within proxy idle limits.

#### Tenant Resource-Creation Safety Caps

- [ ] `p1` - **ID**: `cpt-cf-evbk-nfr-tenant-rate-caps`

Before the full post-MVP quotas feature exists, the broker MUST enforce interim per-tenant safety-floor caps on resource-creation endpoints: `POST /v1/consumer-groups`, `POST /v1/producers`, and `POST /v1/subscriptions`. These caps are abuse controls for resource creation only; they do not replace the deferred full quota model for produce rate, poll rate, active-subscription caps, or storage quotas.

#### Group Cap Per Delivery Instance

- [ ] `p1` - **ID**: `cpt-cf-evbk-nfr-group-cap`

Maximum consumer groups owned per delivery instance: 1000 (default, configurable per deployment). When all delivery instances are at cap, JOIN MUST return `503` with `Retry-After` (operator's signal to scale out).

#### Cluster Mode Failover Bound

- [ ] `p1` - **ID**: `cpt-cf-evbk-nfr-failover-bound`

Convergence on a delivery instance's death (cluster-mode failover) MUST be bounded by `DirectoryService`'s instance heartbeat-loss detection. **The 10–30 second figure previously stated here was the cluster gear's service-discovery TTL; that primitive has been removed and the equivalent bound for `DirectoryService` needs confirming by the broker team before this requirement is treated as sized.**

#### Throughput Efficiency

- [ ] `p1` - **ID**: `cpt-cf-evbk-nfr-throughput-efficiency`

Absolute throughput is set by the chosen storage backend's transaction ceiling, which the broker does not own. The broker-level requirement is therefore stated as **efficiency relative to that ceiling**, not as an absolute events/sec figure:

- **Per-event sustained throughput** SHOULD preserve at least `η = 0.4` of the backend's per-`(topic, partition)` transaction rate (the residual is ingest-outbox and dedup overhead).
- **Batch publish** SHOULD coalesce at least `B = 20` events per backend transaction.

These targets are measured under stated conditions: small payloads (≈1 KiB, not the 64 KiB ceiling), per single `(topic, partition)`, and at the **sustained** rate. Sustained throughput is distinct from the **accept** rate: the asynchronous publish path acknowledges at ingest-outbox-durability speed (higher, burst-absorbing), while sustained throughput is bounded by backend persistence; accept exceeding sustained grows outbox lag and MUST be bounded by lag, not by refusing the accept.

*Illustrative*: against a storage backend rated at ≥5,000 transactions/sec, these factors yield roughly **2,000 events/sec** single-event and **100,000 events/sec** batched. Actual figures depend on backend and topology; the derivation is in DESIGN §3.6.

### 6.2 NFR Exclusions

- **Per-tenant produce rate ceilings, per-subscription poll rate ceilings, per-tenant storage quotas** — deferred to a post-MVP "Quotas and rate limiting" feature. (Per-tenant safety-floor caps on resource-creation endpoints — `POST /v1/consumer-groups`, `POST /v1/producers`, `POST /v1/subscriptions` — are tracked separately as `cpt-cf-evbk-nfr-tenant-rate-caps` and land via the BACKLOG.md §6.1 work.)
- **Absolute events/sec throughput targets** — superseded by `cpt-cf-evbk-nfr-throughput-efficiency`, which states throughput as backend-relative efficiency rather than an absolute figure (absolute numbers depend on the chosen backend and topology).
- **Specific p99 publish / poll latency targets** — publish-accept latency is bounded by ingest-outbox durability (a local write), but end-to-end persist/poll latency depends on the chosen backend and topology, so no absolute p99 is set at the broker level.
- **Dynamic type-pattern refresh in subscriptions** — newly-registered event types matching an active subscription's patterns don't auto-extend the resolved set; consumer re-JOINs. Future `dynamic_types_refresh` flag, post-MVP. Per ADR-0005 § More Information.
- **Second built-in filter engine** — CEL is the v1 default; starlark / rego / vendor-custom are plugins via the GTS-typed registry. Per ADR-0005.
- **Implicit derived-type coverage** (GTS spec §3.6 auto-matching) — v1 requires explicit patterns / full identifiers. Future opt-in. Per ADR-0005 § Acknowledged GTS Features.
- **Runtime-configurable filter limits** — `MAX_INTERESTS_PER_SUBSCRIPTION` / `MAX_TYPES_PER_INTEREST` / `MAX_EXPRESSION_LENGTH_BYTES` / `MAX_COMPILED_FILTER_BYTES` / `EVAL_TIMEOUT_MICROS` are compile-time constants in v1. Per ADR-0005.

## 7. Public Library Interfaces

### 7.1 Public API Surface

The Event Broker exposes its capability via a versioned REST API. The full surface:

```
POST   /v1/events                                # produce single event
POST   /v1/events:batch                          # produce batch (atomic; same topic+partition)
GET    /v1/events:stream?subscription_id=...       # streaming consume

POST   /v1/producers                             # register producer (server-issued id)
GET    /v1/producers/{producer_id}/cursors       # read broker's last_sequence per (topic, partition)

POST   /v1/consumer-groups                       # register anonymous consumer group
GET    /v1/consumer-groups/{id}                  # read consumer group registry record
GET    /v1/consumer-groups                       # list groups visible to caller
DELETE /v1/consumer-groups/{id}                  # remove consumer group from registry

POST   /v1/subscriptions                         # JOIN (create subscription, claim group, get assignments)
POST   /v1/subscriptions/{id}:seek               # seek (set cursor.offset, forward-only during streaming)
DELETE /v1/subscriptions/{id}                    # leave (explicit teardown)
GET    /v1/subscriptions                         # list active subscriptions for caller
GET    /v1/subscriptions/{id}                    # read subscription state

GET    /v1/topics                                # list registered topics
GET    /v1/topics/{id}                           # read topic metadata
GET    /v1/event-types                           # list registered event types
GET    /v1/event-types/{id}                      # read event-type schema and allowed subject types
```

### 7.2 External Integration Contracts

**Producer SDK trait** — `cf-gears-event-broker-sdk` will export a `EventBrokerProducer` trait whose concrete implementation wraps the `toolkit-db` outbox so producers atomically commit business state and event-publish intent in one transaction. Trait shape, error types, retry policies, and reconnection semantics are implementation-phase deliverables.

**Consumer SDK trait** — `cf-gears-event-broker-sdk` will export a `EventBrokerConsumer` trait covering JOIN / poll / seek / leave with normative behavior on `410 Gone` / `404 SubscriptionNotFound` / `412 SequenceViolation` / `409 PartitionNotAssigned` (re-JOIN, drop in-flight, etc.). Per-language SDK realization is implementation-phase work.

**Storage Backend plugin contract** — third-party plugins implement `StorageBackend` and register their GTS type as an extension of `gts.cf.core.events.backend.v1~`. Resolution happens via ClientHub at runtime; no broker-core modification needed.

## 8. Use Cases

#### Audit Pipeline

- [ ] `p1` - **ID**: `cpt-cf-evbk-usecase-audit-pipeline`

**Actor**: `cpt-cf-evbk-actor-module-dev-consumer`

**Preconditions**:
- A named consumer group `gts.cf.core.events.consumer_group.v1~vendor.audit.pipeline.v1` is registered in `types_registry`.
- The audit-pipeline principal holds `:consume` PEP grants on the relevant audit topics, event types, subject types, and the consumer group.

**Main Flow**:
1. A platform module emits an audit event (e.g., `gts.cf.core.events.event.v1~vendor.audit.user_changed.v1~`) on every privileged action.
2. The audit-pipeline module JOINs the named consumer group and streams.
3. The broker delivers events at-least-once; audit-pipeline processes idempotently into long-term storage.
4. Audit-pipeline seeks past processed events via SEEK (`POST /v1/subscriptions/{id}:seek`); broker advances `cursor.offset` for the group.

**Ordering**: Because the default partition key is `tenant` (per [ADR-0002](ADR/0002-partition-selection.md)), all of a tenant's audit events land on one partition and are delivered in publish order — the pipeline observes a **per-tenant total order** of audit events without any dedicated single-partition topic. Cross-tenant order is unspecified, which is acceptable for audit (records are inspected within a tenant); a deployment needing one global ordered stream would use a single-partition audit topic.

**Postconditions**:
- Each privileged action has a corresponding audit-storage record (at-least-once, idempotent), with per-tenant ordering preserved.

**Alternative Flows**:
- **Cross-tenant audit**: multiple tenants' audit events flow through the same named group; per-event `tenant_id` is preserved for downstream filtering, and each tenant's events remain individually ordered (separate partitions).

#### Real-Time Notification Fan-out

- [ ] `p1` - **ID**: `cpt-cf-evbk-usecase-notification-fanout`

**Actor**: `cpt-cf-evbk-actor-module-dev-consumer`

**Preconditions**:
- A topic exists with a high-volume event stream (e.g., chat messages).
- Multiple downstream modules (notifications, search index, analytics) hold independent consumer groups.

**Main Flow**:
1. The producer publishes events to the topic.
2. Each subscribed group's owning delivery shard receives notifications via the per-partition cache.
3. Iterators in each group's open stream wake, advance, apply per-member filters, and emit matching events as multipart parts.
4. Each consumer module processes independently and acks at its own pace.

**Postconditions**:
- Every subscribed group sees every event matching its filter, at-least-once.
- Fan-out cost is O(active iterators) memory walks rather than O(groups) backend queries.

**Alternative Flows**:
- **Group falls behind**: `evbk_cursor_lag` gauge surfaces the lagging group; operator alerts; group catches up via continued polling.

#### Backfill on New Subscriber

- [ ] `p1` - **ID**: `cpt-cf-evbk-usecase-backfill`

**Actor**: `cpt-cf-evbk-actor-module-dev-consumer`

**Preconditions**:
- A new analytics module is deploying with a fresh consumer group.
- The storage backend has a configurable `earliest_available_offset` per `(topic, partition)`.

**Main Flow**:
1. The new module calls `POST /v1/consumer-groups` (anonymous group) or registers a named group via `types_registry`.
2. On JOIN, the broker assigns partitions and sets the cursor to `earliest_available_offset`.
3. The consumer pulls historical events via the streaming endpoint, processes idempotently, acks as it goes.
4. After catching up to the live tail, the consumer transitions to receiving live events from the per-partition cache.

**Postconditions**:
- The new module has processed every available historical event and is now consuming live.

**Alternative Flows**:
- **Backend retention horizon below acked target**: backend may have purged older events; consumer starts from the backend's current `earliest_available_offset` and the operator accepts the gap.

#### Producer Restart with Persistent Chain State

- [ ] `p2` - **ID**: `cpt-cf-evbk-usecase-producer-restart`

**Actor**: `cpt-cf-evbk-actor-module-dev-producer`

**Preconditions**:
- A producer module previously registered via `POST /v1/producers` and persisted `producer_id` + last-known `sequence` alongside its business data.

**Main Flow**:
1. The producer module restarts and reads its persisted `producer_id` + `last_sequence` from local storage.
2. The producer resumes publishing with the next `sequence` after `last_sequence`.

**Postconditions**:
- Publishing continues with no chain breaks; broker's `evbk_producer_state` advances normally.

**Alternative Flows**:
- **Local DB restored from backup (sequence regressed)**: producer detects the divergence (e.g., business data has rows newer than `last_sequence` claims), calls `GET /v1/producers/{producer_id}/cursors` to read the broker's authoritative view; producer either reconciles its own state or registers a fresh `producer_id` via `POST /v1/producers` to start a new chain.

## 9. Acceptance Criteria

- All P1 functional requirements (5.1–5.7) implemented and covered by tests at the levels documented in DESIGN.md.
- All P1 NFRs (6.1) verifiable via integration tests or static configuration assertions.
- The DESIGN.md document is in canonical place (`modules/system/event-broker/docs/DESIGN.md`) and is the source of truth for technical decisions.
- The partition-selection ADR is in canonical place (`modules/system/event-broker/docs/ADR/0002-partition-selection.md`).
- Standalone deployment (single process, in-process cluster provider, in-memory backend) functions end-to-end against the producer + consumer SDKs.
- Cluster deployment (multi-process; persistent cluster provider; postgres backend) functions end-to-end with at least 2 ingest, 2 delivery, 1 dispatcher.
- All authorization decisions go through `authz-resolver`'s PEP — no broker-local authz rules.

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| `toolkit-db` outbox | Transactional outbox foundation for both producer SDK and ingest pipeline (`cpt-cf-evbk-actor-modkit-db`). | p1 |
| `cluster` system module | KV-with-TTL, leader election, distributed locks (`ClusterCacheV1`, `LeaderElectionV1`, `DistributedLockV1`) — `cpt-cf-evbk-actor-cluster`. | p1 |
| `authz-resolver` | `PolicyEnforcer` PEP for all authorization decisions (`cpt-cf-evbk-actor-authz-resolver`). | p1 |
| `tenant-resolver` | Tenant identity, hierarchy resolution, `ROOT_TENANT_ID` (`cpt-cf-evbk-actor-tenant-resolver`). | p1 |
| `types-registry` | GTS schema and instance registration; named consumer-group provisioning at startup (`cpt-cf-evbk-actor-types-registry`). | p1 |
| `api_ingress` | REST API hosting at the platform's HTTP entry point. | p1 |
| `toolkit-security` | Bearer token authentication. | p1 |

## 11. Assumptions

- The platform's `cluster` system module is operational and provides KV-with-TTL, leader election, and distributed lock primitives (verified during design).
- Consumers and producers are responsible for idempotent processing at their layer — the broker's at-least-once contract presumes this.
- `types_registry` is operational at broker startup so named consumer groups can be upserted on launch.
- Storage backend instances bound to topics remain operational; backend-failure backpressure is post-MVP.
- The platform's authentication layer populates `SecurityContext` reliably; the broker reads tenant + principal from it without further validation.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Storage backend selection lock-in | A topic's bound backend cannot be changed without data migration tooling (post-MVP). Selecting an inappropriate backend at topic registration may require a full topic rebuild for migration. | Document backend characteristics and capacity ceilings clearly at registration time; restrict ad-hoc backend choices; prefer postgres unless an explicit scale or compliance constraint forces an alternative. Backend instance migration tooling is on the post-MVP roadmap. |
| Consumer-cursor durability under cache-only model | In standalone mode, broker runtime cursors die with the process; consumers may re-process from their chosen start position after restart unless they persist progress themselves. | Idempotent consumer processing is a normative SDK requirement (per the at-least-once contract). Consumers that need durable progress persist offsets in their own store; broker runtime cursor state is not the durable source of truth. |
| Hot-topic scaling in sharded ingest mode | A single hot topic pinned to a single ingest instance via `serves_topic_patterns` becomes a bottleneck and a noisy neighbor for any topics co-located with it. | Hetero ingest mode (default) load-balances all topics across all ingest instances. Sharded mode is opt-in for explicit isolation; operators choosing sharded mode are responsible for capacity planning per shard. Partition-sharded ingest (sub-topic level) is on the post-MVP roadmap. |
| Schema evolution gap | v1's "new GTS identifier per change" rule makes long-lived event logs operationally awkward without the post-MVP compatible-evolution feature. | Consumers use type-pattern wildcards (`gts.cf.core.events.event.v1~vendor.audit.*`) to subscribe to families of versions. Compatible schema evolution + inter-version casting is on the post-MVP roadmap. |
| Abuse / resource exhaustion without quotas | Without rate limits and storage quotas, a misbehaving or malicious tenant can flood ingest, exhaust backend storage, or starve consumers. The limits **must** be set before broad production exposure. | Interim per-tenant safety-floor caps on resource-creation endpoints (`cpt-cf-evbk-nfr-tenant-rate-caps`) bound the worst vector now. Full **Quotas & Rate Limiting** — scoped per topic / per producer / per consumer (produce rate, poll rate, active-subscription caps, storage quotas) — is the immediate post-MVP feature and needs dedicated design; tracked in BACKLOG.md. |

## 13. Open Questions

- **Outbox extension for partition-level scaling** (deferred): should `toolkit-db` outbox grow native partitioning so a topic with N partitions has N outbox sub-queues? Decision deferred to post-MVP, gated by the outbox library's roadmap.
- **Long-poll protocol revisit**: the per-partition cache + condvar-driven iterator model needs a focused pass after broader feedback closes — cache eviction policy, backfill semantics, fairness across waiters.
- **Tenant-scoped GTS namespaces** for consumer groups: not in MVP; future design may introduce per-tenant namespaces (e.g., `gts.cf.core.events.consumer_group.v1~tenant.{tenant_id}.foo.v1`).

## 14. Traceability

- DESIGN.md — technical realization of every requirement in §5; cross-references via section IDs (`cpt-cf-evbk-*`).
- ADR/0002-partition-selection.md — architectural decision behind partition selection and rationale.
- Future ADR extractions — follow-up design iterations for the remaining DESIGN.md §1.2 ADR-IDs (cluster-capabilities, streaming-delivery, topic-sharding, dispatcher, sequence-assignment, outbox-ingest).
- Future DECOMPOSITION.md — feature phasing and MVP boundaries; final design iteration in this sequence.
