# Event Broker — Scenario Guide

Practical companion to [DESIGN.md](../docs/DESIGN.md) and [openapi.yaml](../docs/openapi.yaml). Each scenario is a concrete HTTP exchange: a literal request, the expected response, and the side effects the broker must produce.

Organized in two parts:

1. **How To** — the integration journey and per-area reference with mechanism summaries.
2. **Guardrails** — rejections, negative cases, and error shapes.

---

## 1. How To

### Integration Journey

To publish and consume events through the broker, follow these steps. Each step links to the scenario(s) with full coverage.

#### Step 1 — Establish a consumer group

Anonymous groups are broker-minted via REST. Named groups are registered via `types_registry` at startup — no REST call needed.

- [Create anonymous group](consumer/groups/1.01-positive-create-anonymous-group.md) — `POST /v1/consumer-groups` → broker mints GTS id; distribute it to your consumer fleet out-of-band.
- [JOIN a named group](consumer/groups/1.08-positive-named-group-join.md) — no creation step; JOIN with the well-known GTS identifier directly.

#### Step 2 — Publish events

Producers submit typed events to a topic. Default is async (`202`); opt into sync persistence with `Sync-Wait`.

- [Publish a single event (async)](producer/single/1.01-positive-publish-single-async.md) — `POST /v1/events` → `202 Accepted`.
- [Publish sync (wait=persisted)](producer/single/1.02-positive-publish-sync-wait-persisted.md) — `POST /v1/events` with `Sync-Wait` header → `201 Created`.

#### Step 3 — JOIN a subscription

A consumer instance JOINs the group with topic interests and receives its partition assignment.

- [Cold JOIN, fresh group](consumer/subscriptions/1.01-positive-cold-join-fresh-group.md) — `POST /v1/subscriptions` → `201` with `assigned[]` + `topology_version`.

#### Step 4 — SEEK the starting position

Before streaming, the consumer declares where each assigned partition begins. **This step is required** — opening the stream without it returns `409 Failed Precondition`.

**Path A — Consumer has a persistent store (service DB, browser IndexedDB, filesystem)**

Read the last processed offset from your own store and SEEK with the exact integer:

- [SEEK to exact offset](consumer/positions/1.03-positive-seek-exact-offset.md) — `POST /v1/subscriptions/{id}:seek` with integer offset from own DB.
- [SEEK to timestamp](consumer/positions/1.11-positive-seek-at-timestamp.md) — use `"at:<ISO-8601>"` sentinel for date-anchored readers; response returns resolved integer for persistence.
- [Full Path A journey](consumer/flows/1.03-flow-path-a-consumer-with-db.md) — end-to-end: JOIN → SEEK from DB → stream → persist offset → reconnect resumes correctly.

**Path B — No persistent store (one-shot scripts, stateless readers)**

SEEK with a sentinel; accept reprocessing on restart:

- [SEEK to earliest](consumer/positions/1.01-positive-seek-earliest.md) — `"earliest"` → cursor = RF − 1 (= 0 on a fresh topic where RF = 1; always ≥ 0); broker emits from RF.
- [SEEK to latest](consumer/positions/1.02-positive-seek-latest.md) — `"latest"` → broker resolves to current HWM; only future events delivered.

#### Step 5 — Stream events

Open the long-lived multipart stream and consume frames as they arrive.

- [Stream multipart frames](consumer/stream/1.01-positive-stream-multipart-frames.md) — `GET /v1/events:stream` → `200 multipart/mixed`; one event per part.
- [SSE event stream](consumer/stream/1.09-positive-sse-event-stream.md) — `GET /v1/events:sse` → `200 text/event-stream`; browser-native alternative.

> **End-to-end**: [Publish → subscribe → consume](flows/1.01-flow-publish-subscribe-consume.md) composes all steps into one coupled transcript.

---

### Parallel consumer group formation

Multiple consumer instances share a group by JOINing with the same `consumer_group` identifier. The broker rebalances partitions across all active members automatically.

**Anonymous group**: one instance calls `POST /v1/consumer-groups` and distributes the returned id out-of-band (shared DB row, ConfigMap, env var). All instances then JOIN with that id.

**Named group**: no creation step. All instances JOIN with the well-known GTS identifier — no coordination needed.

- [Second JOIN triggers rebalance](consumer/subscriptions/1.11-positive-second-join-triggers-rebalance.md) — 2nd member joins; `topology` frame splits 4 partitions 2+2.
- [Third JOIN triggers rebalance](consumer/subscriptions/1.12-positive-third-join-triggers-rebalance.md) — 3rd member joins; three-way partition split.

---

### producer/ — Publish events

#### producer/single/ — `POST /v1/events` (stateless)
- [positive-1.1 — Publish single (async)](producer/single/1.01-positive-publish-single-async.md) — `POST /v1/events` → `202 Accepted`; event enqueued in outbox.
- [positive-1.2 — Publish sync (wait=persisted)](producer/single/1.02-positive-publish-sync-wait-persisted.md) — `POST /v1/events` with `Sync-Wait` → `201 Created` after backend persist.
- [negative-1.5 — Read-only partition rejected](producer/single/1.05-negative-readonly-partition-rejected.md) — producer-supplied `partition` on publish → `400 Bad Request`; broker derives partition itself.

#### producer/batch/ — `POST /v1/events:batch`
- [positive-1.1 — Publish batch](producer/batch/1.01-positive-publish-batch.md) — `POST /v1/events:batch` → `202 Accepted`; all-or-nothing per topic+partition.

#### producer/flows/ — `POST /v1/producers`, `GET /cursors`, `POST :reset`
- [positive-1.1 — Register chained producer](producer/flows/1.01-positive-register-chained-producer.md) — `POST /v1/producers { mode: chained }` → `201` with `producer_id`.
- [positive-1.2 — Register monotonic producer](producer/flows/1.02-positive-register-monotonic-producer.md) — `POST /v1/producers { mode: monotonic }` → `201`.
- [positive-1.3 — Chained-mode sequence](producer/flows/1.03-positive-chained-mode-sequence.md) — `POST /v1/events` with `Producer-Id` header and `meta.previous/sequence`; broker deduplicates.
- [positive-1.4 — Idempotency key dedup](producer/flows/1.04-positive-idempotency-key-dedup.md) — duplicate event id returns `200` with original event; no second write.
- [positive-1.6 — Cursor recovery](producer/flows/1.06-positive-cursor-recovery.md) — `GET /v1/producers/{id}/cursors` → `{producer_id, client_agent, topics:[{topic, partitions:[{partition, last_sequence}]}]}`; feeds next SEEK after desync.
- [positive-1.7 — Chain reset](producer/flows/1.07-positive-chain-reset.md) — `POST /v1/producers/{id}:reset` → `200`; chain state cleared, audited.
- [flow-1.9 — Desync recovery](producer/flows/1.09-flow-chained-producer-desync-recovery.md) — producer loses sequence state; first publish fails `412`; reads `/cursors`; reconciles counter; republishes successfully.

---

### consumer/ — Consume events

#### consumer/groups/ — `POST/GET/DELETE /v1/consumer-groups`
- [positive-1.1 — Create anonymous group](consumer/groups/1.01-positive-create-anonymous-group.md) — `POST /v1/consumer-groups` → `201` with broker-minted GTS id.
- [positive-1.2 — Get group by id](consumer/groups/1.02-positive-get-group-by-id.md) — `GET /v1/consumer-groups/{id}` → full group record.
- [positive-1.3 — List groups](consumer/groups/1.03-positive-list-groups.md) — `GET /v1/consumer-groups` → paged list of caller-visible groups.
- [positive-1.4 — Delete empty group](consumer/groups/1.04-positive-delete-empty-group.md) — `DELETE /v1/consumer-groups/{id}` → `204`; only when no active subscriptions.
- [positive-1.8 — Named group JOIN (no create step)](consumer/groups/1.08-positive-named-group-join.md) — JOIN with `types_registry`-provisioned identifier; broker validates `:consume` grant.
- [negative-1.9 — Cross-tenant group access denied](consumer/groups/1.09-negative-cross-tenant-anonymous-group-ownership.md) — GET/DELETE/LIST from a different tenant → `403 Permission Denied`; anonymous group ownership is tenant-scoped.

#### consumer/subscriptions/ — `POST/GET/DELETE /v1/subscriptions`
- [positive-1.1 — Cold JOIN, fresh group](consumer/subscriptions/1.01-positive-cold-join-fresh-group.md) — `POST /v1/subscriptions` → `201` with `assigned[]` and `topology_version`.
- [positive-1.2 — Multi-topic interests](consumer/subscriptions/1.02-positive-join-multi-topic-interests.md) — JOIN with interests across two topics; partition assignment spans both.
- [positive-1.3 — Typed filter](consumer/subscriptions/1.03-positive-join-with-typed-filter.md) — `interest.filter { engine, expression }` applied per-member at delivery.
- [positive-1.4 — Multiple subscriptions (parallelism)](consumer/subscriptions/1.04-positive-parallelism-multiple-subscriptions.md) — second JOIN rebalances partitions 2+2.
- [positive-1.5 — Leave subscription](consumer/subscriptions/1.05-positive-leave-subscription.md) — `DELETE /v1/subscriptions/{id}` → `204`; triggers rebalance.
- [positive-1.9 — List subscriptions](consumer/subscriptions/1.09-positive-list-subscriptions.md) — `GET /v1/subscriptions` → paged list of active subscriptions.
- [positive-1.10 — Read subscription](consumer/subscriptions/1.10-positive-read-subscription.md) — `GET /v1/subscriptions/{id}` → current assignment and expiry.
- [positive-1.11 — Second JOIN triggers rebalance](consumer/subscriptions/1.11-positive-second-join-triggers-rebalance.md) — 2nd instance joins; partitions split 2+2; `topology` frame emitted.
- [positive-1.12 — Third JOIN triggers rebalance](consumer/subscriptions/1.12-positive-third-join-triggers-rebalance.md) — 3rd instance joins; three-way split; all streams updated.

#### consumer/positions/ — `POST /v1/subscriptions/{id}:seek` (SEEK)
- [positive-1.1 — SEEK earliest](consumer/positions/1.01-positive-seek-earliest.md) — `"earliest"` → cursor = RF − 1 (= 0 on a fresh topic where RF = 1; always ≥ 0); broker emits from RF onwards.
- [positive-1.2 — SEEK latest](consumer/positions/1.02-positive-seek-latest.md) — `"latest"` → cursor set to HWM; only future events delivered.
- [positive-1.3 — SEEK exact offset](consumer/positions/1.03-positive-seek-exact-offset.md) — integer last-processed offset from own DB; broker emits from `offset + 1`.
- [positive-1.4 — Mixed sentinels and integers](consumer/positions/1.04-positive-mixed-sentinels.md) — partitions may mix `"earliest"`, `"latest"`, and exact offsets in one request.
- [positive-1.10 — Any value in retention range](consumer/positions/1.10-positive-seek-any-value-in-range.md) — any integer in `[RF−1, HWM]` is accepted.
- [positive-1.11 — SEEK at timestamp](consumer/positions/1.11-positive-seek-at-timestamp.md) — `"at:<ISO-8601>"` → resolves to first event at or after timestamp; response returns integer.
- [positive-1.12 — Timestamp before retention](consumer/positions/1.12-positive-seek-at-timestamp-before-retention.md) — timestamp before RF → clamps to RF.
- [positive-1.13 — Timestamp beyond HWM](consumer/positions/1.13-positive-seek-at-timestamp-beyond-hwm.md) — future timestamp → resolves to HWM (equivalent to `"latest"`).
- [positive-1.14 — Resume from persisted cursor](consumer/positions/1.14-positive-seek-resume-from-cursor.md) — consumer reads to sequence 40, persists cursor, reconnects with SEEK to 40; first event on resumed stream is sequence 41.

#### consumer/stream/ — `GET /v1/events:stream`, `GET /v1/events:sse`
- [positive-1.1 — Multipart frames](consumer/stream/1.01-positive-stream-multipart-frames.md) — `GET /v1/events:stream` → `200 multipart/mixed`; `event`, `heartbeat`, `advisory`, `topology` frame kinds.
- [positive-1.2 — Heartbeat cadence](consumer/stream/1.02-positive-stream-heartbeat-cadence.md) — idle stream emits `heartbeat` every 5 s; keeps connection alive through proxies.
- [positive-1.3 — Topology frame on rebalance](consumer/stream/1.03-positive-stream-topology-frame-on-rebalance.md) — mid-stream JOIN by another member triggers `topology` frame with new `assigned[]`.
- [positive-1.9 — SSE event stream](consumer/stream/1.09-positive-sse-event-stream.md) — `GET /v1/events:sse` → `200 text/event-stream`; same frame schema as multipart.
- [positive-1.12 — Partition gain terminates subscription](consumer/stream/1.12-positive-stream-terminates-on-rebalance-gain.md) — surviving member gains freed partitions; broker sends `control/terminal`; consumer re-JOINs and re-SEEKs.
- [positive-1.13 — DELETE while streaming](consumer/stream/1.13-positive-delete-while-streaming.md) — `DELETE /v1/subscriptions/{id}` while stream is open closes the connection immediately; no final control frame emitted.
- [positive-1.14 — Control progress frame](consumer/stream/1.14-positive-control-progress-frame.md) — heavily-filtered stream emits `control/progress` so the consumer can advance its offset store without re-scanning.

#### consumer/flows/ — consumer-only end-to-end journeys
- [flow-1.1 — Two-consumer rebalance](consumer/flows/1.01-flow-two-consumer-rebalance.md) — full inline transcript: consumer A holds all partitions; B joins; rebalance; both stream.
- [flow-1.2 — PositionsNotSet recovery](consumer/flows/1.02-flow-positions-not-set-recovery.md) — SDK mis-SEEKs; broker returns `409`; SDK re-SEEKs and resumes.
- [flow-1.3 — Path A consumer with DB](consumer/flows/1.03-flow-path-a-consumer-with-db.md) — consumer reads own DB → SEEK exact offset → stream → persist offset → reconnect resumes from correct position.
- [flow-1.4 — Leave triggers gain then terminate](consumer/flows/1.04-flow-leave-triggers-gain-terminate.md) — B leaves; A gains B's partitions; A receives `control/terminal` and re-JOINs with the full assignment.

---

### topics/ — Topic introspection

- [positive-1.1 — List topics](topics/1.01-positive-list-topics.md) — `GET /v1/topics` → paged list of topic DTOs carrying `id`, `description`, `retention`.
- [positive-1.2 — List topic segments](topics/1.02-positive-list-topic-segments.md) — `GET /v1/topics/segments?topic=...&partition=...` → segment manifest with RF/HWM per segment.
- [positive-1.4 — List event types](topics/1.04-positive-list-event-types.md) — `GET /v1/event-types?topic=...` → paged list of event type registrations.

---

### flows/ — Coupled producer + consumer journeys

- [flow-1.1 — Publish → subscribe → consume](flows/1.01-flow-publish-subscribe-consume.md) — producer publishes 3 events; consumer creates group, JOINs, SEEKs, streams, processes.

---

## 2. Guardrails

### Auth & permissions

- [negative-1.1 — Missing bearer token](auth/1.01-negative-missing-bearer-token.md) — no `Authorization` header → `401 Unauthenticated` on any endpoint.
- [negative-1.2 — Invalid bearer token](auth/1.02-negative-invalid-bearer-token.md) — expired or malformed token → `401 Unauthenticated`.
- [negative-1.3 — No produce permission](auth/1.03-negative-insufficient-permission-produce.md) — `POST /v1/events` without `topic:produce` → `403 Permission Denied`.
- [negative-1.4 — No consume permission](auth/1.04-negative-insufficient-permission-consume.md) — `POST /v1/subscriptions` without `topic:consume` → `403 Permission Denied`.
- [negative-1.5 — Cross-tenant anonymous group](auth/1.05-negative-cross-tenant-anonymous-group.md) — tenant B JOINs tenant A's anonymous group → `403 Permission Denied`.
- [negative-1.6 — Unauthorized topic JOIN](consumer/subscriptions/1.06-negative-join-unauthorized-topic.md) — interest references a topic the principal cannot consume → `403`.

### Input validation

- [negative-1.3 — Schema validation failure](producer/single/1.03-negative-schema-validation-failure.md) — `event.data` fails JSON Schema → `422 Invalid Argument`.
- [negative-1.5 — Read-only partition rejected](producer/single/1.05-negative-readonly-partition-rejected.md) — producer-supplied `partition` on publish → `400 Bad Request`; `partition` is consumer-facing/read-side only.
- [negative-1.2 — Mixed-partition batch](producer/batch/1.02-negative-mixed-partition-batch.md) — batch events span different partitions → `400 Invalid Argument`.
- [negative-1.3 — Batch too large](producer/batch/1.03-negative-batch-too-large.md) — over 100 events or 1 MiB → `400 Invalid Argument`.
- [negative-1.4 — Batch late validation failure](producer/batch/1.04-negative-batch-late-validation-failure.md) — later invalid event rejects whole batch → `422 Invalid Argument`.
- [negative-1.7 — Too many interests](consumer/subscriptions/1.07-negative-join-too-many-interests.md) — more than 64 interests in one JOIN → `400 Invalid Argument`.
- [negative-1.6 — Invalid client_agent](consumer/groups/1.06-negative-invalid-client-agent.md) — non-ASCII or oversized `client_agent` → `400 Invalid Argument`.
- [guardrail-1.7 — Stream requires multipart Accept](consumer/stream/1.07-guardrail-stream-accept-json-rejected.md) — `Accept: application/json` on `:stream` endpoint → `406 Invalid Argument`.
- [guardrail-1.8 — SSE from multipart endpoint](consumer/stream/1.08-guardrail-sse-from-stream-endpoint.md) — `Accept: text/event-stream` on `/events:stream` → `406`; use `/events:sse` instead.
- [negative-1.10 — Stream rejects timeout/collect params](consumer/stream/1.10-negative-stream-rejects-timeout-collect-params.md) — unsupported query params on `:stream` → `400 Invalid Argument`.

### Seek / cursor errors

- [negative-1.5 — Out-of-range offset](consumer/positions/1.05-negative-out-of-range-offset.md) — offset below RF−1 → `400 Invalid Argument`.
- [negative-1.6 — Offset above HWM](consumer/positions/1.06-negative-offset-above-hwm.md) — offset beyond HWM → `400 Invalid Argument`.
- [negative-1.7 — SEEK while streaming](consumer/positions/1.07-negative-seek-while-streaming.md) — any SEEK while `:stream` is open → `409 StreamingInProgress` (SEEK is pre-stream-only).
- [negative-1.8 — SEEK unknown subscription](consumer/positions/1.08-negative-seek-unknown-subscription.md) — `:seek` on a non-existent `subscription_id` → `404 Not Found`.
- [negative-1.9 — SEEK unassigned partition](consumer/positions/1.09-negative-seek-unassigned-partition.md) — SEEK references a partition not in `assigned[]` → `409 Failed Precondition`.

### Stream errors

- [negative-1.4 — PositionsNotSet](consumer/stream/1.04-negative-stream-positions-not-set.md) — stream opened without prior SEEK → `409 Failed Precondition`; `context.unseeded` lists affected partitions.
- [negative-1.5 — Unknown subscription](consumer/stream/1.05-negative-stream-unknown-subscription.md) — `subscription_id` not found or expired → `404 Not Found`.
- [negative-1.6 — Terminated subscription](consumer/stream/1.06-negative-stream-terminated-subscription.md) — delivery shard shutdown sends `410`; consumer re-JOINs.
- [negative-1.11 — Second stream rejected](consumer/stream/1.11-negative-streaming-in-progress.md) — second `:stream` open while one is already active → `409 StreamingInProgress`; existing stream unaffected.

### Producer chain errors

- [negative-1.5 — Chained sequence violation](producer/flows/1.05-negative-chained-sequence-violation.md) — `meta.previous` doesn't match broker's `last_sequence` → `412 Failed Precondition`; recover via `GET /v1/producers/{id}/cursors`.
- [negative-1.8 — Unknown producer](producer/flows/1.08-negative-unknown-producer.md) — `Producer-Id` not registered or reaped → `400 Invalid Argument`.

### Consumer group errors

- [negative-1.5 — Delete group with active members](consumer/groups/1.05-negative-delete-group-with-active-members.md) — `DELETE` while subscriptions exist → `409 Failed Precondition`.
- [negative-1.7 — Get unknown group](consumer/groups/1.07-negative-get-unknown-group.md) — `GET /v1/consumer-groups/{id}` for non-existent id → `404 Not Found`.
- [negative-1.8 — LEAVE unknown subscription](consumer/subscriptions/1.08-negative-leave-unknown-subscription.md) — `DELETE /v1/subscriptions/{id}` for expired/unknown id → `404 Not Found`.
- [negative-1.9 — Cross-tenant group access denied](consumer/groups/1.09-negative-cross-tenant-anonymous-group-ownership.md) — GET/DELETE/LIST from a different tenant → `403 Permission Denied`; anonymous group ownership is tenant-scoped.
- [negative-1.13 — JOIN group at capacity](consumer/subscriptions/1.13-negative-join-group-at-capacity.md) — all partitions already assigned; new JOIN → `429` with `Retry-After`; zero-partition standby not admitted.

### Topics / segments errors

- [negative-1.3 — Segments for unknown topic](topics/1.03-negative-segments-unknown-topic.md) — `GET /v1/topics/segments` with unregistered topic → `404 Not Found`.

### Rate limiting

- [negative-1.4 — Publish rate limited](producer/single/1.04-negative-rate-limited.md) — publish exceeds per-tenant quota → `429 Resource Exhausted` with `Retry-After`.

### Error envelope reference

- [1.01 — Problem Details envelope](errors/1.01-positive-problem-details-envelope.md) — canonical RFC-9457 + GTS shape; all broker errors use this format.
- [1.02 — 401 Unauthenticated](errors/1.02-negative-401-unauthenticated.md)
- [1.03 — 403 Permission Denied](errors/1.03-negative-403-unauthorized.md)
- [1.04 — 404 Not Found](errors/1.04-negative-404-not-found.md)
- [1.05 — 409 Failed Precondition](errors/1.05-negative-409-conflict.md)
- [1.06 — 412 Failed Precondition (sequence)](errors/1.06-negative-412-sequence-violation.md)
- [1.07 — 429 Resource Exhausted](errors/1.07-negative-429-rate-limited.md)
- [1.08 — 500 Internal](errors/1.08-negative-500-internal.md)

---

## 3. Authoring Rules

### Folder assignment

Put a scenario in the folder that matches what a reader is trying to understand — not which endpoint it calls.

| Question | Folder |
|---|---|
| "How do I publish an event?" | `producer/single/` or `producer/batch/` |
| "How does the idempotent producer protocol work?" | `producer/flows/` |
| "How do I create / manage a group?" | `consumer/groups/` |
| "How do I join / leave a subscription?" | `consumer/subscriptions/` |
| "How do I set or change my position?" | `consumer/positions/` |
| "How does the stream / SSE transport work?" | `consumer/stream/` |
| "What are the topic segment offsets?" | `topics/` |
| "What happens when auth fails?" | `auth/` |
| "What does a specific error code look like?" | `errors/` |
| "Show me a producer-only end-to-end journey" | `producer/flows/` |
| "Show me a consumer-only end-to-end journey" | `consumer/flows/` |
| "Show me publish + consume together" | `flows/` (top-level) |

### Naming convention

`{positive|negative|guardrail}-{area-number}.{seq}-{slug}.md`

Numbers are relative to the sub-area folder (restart at 1.1 for each new folder). Slugs are kebab-case; describe the behavior, not the endpoint.

### Cross-reference format

Use relative paths from the scenario file. Example from `consumer/subscriptions/`:

```markdown
[Create anonymous group](../groups/1.01-positive-create-anonymous-group.md)
```

### Flows placement

| Journey involves | Use |
|---|---|
| Only producer-side exchanges | `producer/flows/` |
| Only consumer-side exchanges | `consumer/flows/` |
| Both producer and consumer in same transcript | `flows/` (top-level) |

### Error format

All error response bodies MUST use the canonical GTS + RFC-9457 shape:

```json
{
  "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.<category>.v1~",
  "title": "<Category Label>",
  "status": <HTTP code>,
  "detail": "<human-readable detail for this occurrence>",
  "instance": "<request path>",
  "context": { "<domain fields>" }
}
```

HTTP status → category mapping:

| Status | Category | `title` |
|---|---|---|
| 400, 422 | `invalid_argument` | `"Invalid Argument"` |
| 401 | `unauthenticated` | `"Unauthenticated"` |
| 403 | `permission_denied` | `"Permission Denied"` |
| 404, 410 | `not_found` | `"Not Found"` |
| 409, 412 | `failed_precondition` | `"Failed Precondition"` |
| 429 | `resource_exhausted` | `"Resource Exhausted"` |
| 500 | `internal` | `"Internal"` |

Domain-specific fields (e.g., `unseeded`, `expected_previous`, `valid_range`) go inside `context`, not at root level.

### Side-effects predicate vocabulary

| Kind | Form |
|---|---|
| State | `<table>(<key>) is set to <value>` · `<table>(<key>) is absent` · `<table>(<key>) advances from <old> to <new>` |
| Frame | `subscription <id> next frame is <kind> with <assertion>` · `subscription <id> emits <kind> within <duration>` |
| Reply | `subsequent <call> returns <code>` |
| Lifecycle | `subscription <id> is reaped after <duration>` · `consumer-group <id> is deleted` |
| Metric / audit | `metric <name> incremented by <n>` · `audit log entry <type> created` |

### Legend

| Shorthand | Meaning |
|---|---|
| `PD` | RFC-9457 Problem Details (`application/problem+json`). Implied by any `4xx` / `5xx`. |
| `RF` | Partition retention floor — smallest offset still readable. |
| `HWM` | Partition high-water mark — offset of the next event to be admitted. |
| `MP` | Multipart frame on `:stream`. |
| `SSE` | Server-Sent Event frame on `:sse`. |
| `Cursor` | Runtime/session cursor value for a `(group, topic, partition)` assignment — last-processed offset; broker emits from `Cursor + 1`. |
