# Transactional Outbox

Reliable async message production with per-partition ordering guarantees.
Supports PostgreSQL, MySQL/MariaDB, and SQLite.

Four-stage pipeline: enqueue (inside your transaction) -> sequencer
(assigns per-partition sequence numbers) -> processor (calls your handler)
-> vacuum (GC). Two processing modes: transactional (exactly-once) and
leased (at-least-once with lease-based locking and framework-managed
cancellation).

## Usage

Default usage keeps the existing `toolkit_outbox_*` tables:

```rust
run_migrations_for_testing(&db, outbox_migrations()).await?;

let handle = Outbox::builder(db)
    .queue("orders", Partitions::of(4))
    .leased(OrderHandler { client })
    .start().await?;
```

### Custom table prefix

Use the same prefix for migrations and the runtime builder. The prefix creates
a complete independent table family by appending fixed suffixes such as
`_body`, `_partitions`, `_incoming`, `_outgoing`, and `_dead_letters`.

```rust
run_migrations_for_testing(
    &db,
    outbox_migrations_with_prefix("mini_chat_outbox")?,
).await?;

let handle = Outbox::builder(db)
    .table_prefix("mini_chat_outbox")?
    .queue("orders", Partitions::of(4))
    .leased(OrderHandler { client })
    .start().await?;
```

Changing the prefix points the outbox at a different table family. It does not
rename tables or move existing rows. To move data between prefixes, drain or
migrate the rows explicitly.

Prefixes are validated before SQL is generated: they must be non-empty ASCII
identifiers, start with a letter, contain only letters, digits, and underscores,
and stay short enough that derived table and index names fit common backend
identifier limits. Schema-qualified input such as `public.outbox`, quoting,
spaces, semicolons, and other punctuation are rejected. Table and index names
are SQL identifiers, so they cannot be bound as query parameters; only validated
identifiers are interpolated, while queue names, payloads, partition IDs, lease
IDs, limits, and filters remain SQL parameters.

At runtime, the validated table names and detected backend are compiled once
into an internal `OutboxStatements` catalog. Fixed-shape SQL is reused from that
catalog instead of being rebuilt through default table-name replacement on every
operation. Dynamic SQL is still built for runtime-cardinality cases such as
multi-row inserts and `IN (...)` lists.

For MySQL, migrations also create `<prefix>_body_id_sequence` and
`<prefix>_incoming_id_sequence`. Batch enqueue reserves body IDs first and
incoming IDs second by locking the singleton row with `SELECT ... FOR UPDATE`,
advancing `next_id`, then inserting rows with explicit IDs. On MySQL-compatible
clusters such as Percona XtraDB Cluster/Galera, retry the whole transaction on
deadlock, serialization, or certification-conflict errors.

### Single-message handler (leased)

```rust
struct OrderHandler {
    client: HttpClient,
}

#[async_trait]
impl LeasedMessageHandler for OrderHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let order: Order = match serde_json::from_slice(&msg.payload) {
            Ok(o) => o,
            Err(e) => return MessageResult::Reject(format!("bad payload: {e}")),
        };
        match self.client.post(&warehouse_url).json(&order).send().await {
            Ok(resp) if resp.status().is_success() => MessageResult::Ok,
            Ok(_) | Err(_) => MessageResult::Retry,
        }
    }
}

let handle = Outbox::builder(db)
.profile(OutboxProfile::low_latency())
.queue("orders", Partitions::of(4))
.leased(OrderHandler { client })
.start().await?;
```

### Transactional handler (exactly-once with DB writes)

```rust
struct AuditHandler;

#[async_trait]
impl TransactionalMessageHandler for AuditHandler {
    async fn handle(
        &self,
        txn: &dyn ConnectionTrait,
        msg: &OutboxMessage,
        _cancel: CancellationToken,
    ) -> HandlerResult {
        // DB writes here are atomic with the ack
        if let Err(e) = audit_log::ActiveModel { payload: Set(msg.payload.clone()), .. }
            .insert(txn).await
        {
            return HandlerResult::Retry { reason: format!("insert failed: {e}") };
        }
        HandlerResult::Success
    }
}

let handle = Outbox::builder(db)
.queue("audit", Partitions::of(2))
.transactional(AuditHandler)
.start().await?;
```

### Enqueue (inside a business transaction)

A submission is built, not constructed as a literal: everything checkable
without the database is checked while it is built, so a rejected submission has
issued no statement.

```rust
let outbox = handle.outbox();

// Atomic with your business logic:
outbox.enqueue(&txn, Record::to("orders", partition)
    .payload(payload, "application/json")
    .build()?).await?;

// A batch states the queue and the payload type once:
outbox.enqueue_batch(&txn, Records::to("orders")
    .payload_type("application/json")
    .push(0, first)
    .push(1, second)
    .push_with_type(2, legacy, "application/vnd.legacy+json")
    .build()?).await?;
```

A batch is all-or-nothing: one entity that breaks a rule rejects the whole
submission, and nothing is written.

### Multi-queue with tuning

```rust
let handle = Outbox::builder(db)
.profile(OutboxProfile::high_throughput())
.sequencer_tuning(WorkerTuning::sequencer_high_throughput().batch_size(500))
.vacuum_tuning(WorkerTuning::vacuum().idle_interval(Duration::from_secs(300)))
.queue("orders", Partitions::of(16))
.leased(OrderHandler { client: client.clone() })
.queue("notifications", Partitions::of(4))
.leased(NotifyHandler { client })
.lease(LeaseConfig {
duration: Duration::from_secs(60),
headroom: Duration::from_secs(5),
})
.start().await?;

// Graceful shutdown
handle.stop().await;
```

---

## Traces: being told when a batch is done

Submit a batch under a trace, and the outbox tells you once every entity in it
has reached a terminal state - handler success or permanent rejection. The
batch is the unit: you are told once, not per message.

```rust
// Subscribe before the transaction commits. A completion cannot precede that
// commit, so registering first means nothing can be missed.
let waiting = outbox.subscribe("import-2026-09-08");

db.in_transaction(|txn| async move {
    orders_repo.insert(txn, &orders).await?;
    outbox.enqueue_batch(txn, Records::to("orders")
        .payload_type("application/json")
        .trace("import-2026-09-08")
        .push(0, first)
        .push(1, second)
        .build()?).await
}).await?;

match waiting.await {
    Some(outcome) if outcome.is_clean() => info!(entities = outcome.entities, "all delivered"),
    Some(outcome) => warn!(failures = outcome.failures, "batch finished with failures"),
    // This process can no longer answer - it was stopped, for instance.
    // The trace row still can; see `trace_status` below.
    None => {}
}
```

Dropping the subscription releases it and issues no statement. A trace is
optional per submission: a batch that names none records nothing and costs
nothing.

### The completion goes to the instance that submitted it

Any instance may process the work, because partitions are leased rather than
owned. Only the instance that submitted the batch is told. Two marks make that
work, with two different owners:

| mark | who sets it | means |
|---|---|---|
| `completed_at` | whichever instance acks the last entity | the work is done |
| `notified_at` | only the submitting instance | you have been told |

When the submitting instance is also the one that finishes the work - the
common case, and every single-instance deployment - the ack delivers the
completion itself and no notification query runs at all. Otherwise the
submitter collects it, and an instance with nothing outstanding issues no query
either.

There is nothing to configure for this. Each running outbox generates its own
identity, which only has to be unique among *running* processes: a restarted
process holds no subscription for a completion to be delivered to, so a stable
name would buy nothing. Mail addressed to a process that is gone is collected
by the sweep, and a restarted process asks `trace_status` instead.

### Being told when a batch is stuck

Waiting on a subscription tells you nothing until the batch is done, which is
no help when it is stuck rather than slow. Ask the subscription for a progress
receiver and you are told while it is in flight:

```rust
let waiting = outbox.subscribe("import-2026-09-08");
let mut stalls = waiting.progress();
tokio::spawn(async move {
    while stalls.changed().await.is_ok() {
        match stalls.borrow_and_update().clone() {
            Some(stalled) => warn!(
                attempts = stalled.attempts,
                for_secs = stalled.stalled_for().num_seconds(),
                error = ?stalled.last_error,
                "import is stuck"
            ),
            None => info!("import is moving again"),
        }
    }
});
let outcome = waiting.await;
```

The receiver holds `None` while the batch is moving and `Some(TraceProgress)`
while a handler is retrying one of its entities; the outbox decides nothing
about what a stall means, which is the point - a batch retrying for ten seconds
against a rate-limited API is healthy, and the same batch retrying for an hour
is not.

Taking a receiver is what makes an instance look for stalls. A caller that only
awaits the completion never takes one, and an instance whose callers all do that
issues no stall query at all.

### Asking instead of waiting

The trace row outlives both the messages it describes and the process that
submitted them, so it is also the durable answer after a restart:

```rust
if let Some(status) = outbox.trace_status(&conn, "import-2026-09-08").await? {
    if status.is_stalled() {
        // A batch stuck retrying rather than merely slow: `attempts`,
        // `stalled_since` and `last_error` say why and for how long.
        warn!(attempts = status.attempts, since = ?status.stalled_since, "import stalled");
    }
}
```

`stalled_since` keeps the *first* stall time rather than the latest attempt, so
what you read is how long it has been stuck. Progress clears it.

Which entities failed is answerable too: a dead letter carries the trace it
belonged to, so the failures of one batch can be listed without deserializing
any payload.

### Trace rows are collected when they are finished

The sweep reads **one table**: liveness comes from the trace row's own fields,
so a five-minute background sweep never touches the body or dead-letter tables.

| condition | action |
|---|---|
| delivered, no failures, past `retention` | collected |
| completed but never collected, past `orphan_after` | collected: the owner is evidently gone |
| delivered **with** failures, past `leftover_after` | collected: a dead letter outlives the delivery it failed, so its trace has to outlive the dead letter |
| never completed, past `leftover_after` | collected |

The last rule is worth knowing about: a trace whose handler has been retrying
for longer than `leftover_after` is collected even though its messages still
exist, and `trace_status` then answers `None` for it. The window is seven days
by default, so this means a handler that has made no progress for a week. The
messages themselves are untouched - only the notification aid is.

## Use-Case Scenarios

### Handler makes a remote HTTP call

The most common case. Implement `LeasedMessageHandler` - one message, one
result. Use `HttpClient` from `cf-gears-toolkit-http` for the outgoing call.

**Cancellation is framework-managed.** The processor drops the handler
future when the lease cancel point is reached (`lease_duration - ack_headroom`).
In-flight `HttpClient` calls are cancelled via drop - hyper closes the
connection.

```rust
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_http::HttpClient;

pub struct WebhookHandler {
    client: HttpClient,
    url: String,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for WebhookHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: Event = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("bad payload: {e}")),
        };

        // HttpClient handles per-attempt timeout (30s) and retries (3x).
        // The Idempotency-Key header enables POST retry in the retry layer.
        let idempotency_key = format!("{}:{}", msg.partition_id, msg.seq);

        match self.client
            .post(&self.url)
            .header("Idempotency-Key", &idempotency_key)
            .json(&event)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => MessageResult::Ok,
            Ok(_) | Err(_) => MessageResult::Retry,
        }
    }
}
```

**Idempotency is required.** Leased processing provides at-least-once
delivery. If the lease expires before the ack transaction, the message is
re-delivered. The `Idempotency-Key` header (derived from `partition_id`
and `seq`) also enables `HttpClient` retry for POST/PATCH requests.

### Handler makes multiple sequential calls

When a single message requires several calls (enrich -> transform ->
publish), use `tokio::time::timeout_at` with a shared deadline:

```rust
#[async_trait::async_trait]
impl LeasedMessageHandler for PipelineHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);

        let enriched = match tokio::time::timeout_at(
            deadline,
            self.enrichment_api.enrich(msg),
        ).await {
            Ok(Ok(data)) => data,
            _ => return MessageResult::Retry,
        };

        let transformed = match tokio::time::timeout_at(
            deadline,
            self.transform_api.transform(&enriched),
        ).await {
            Ok(Ok(data)) => data,
            _ => return MessageResult::Retry,
        };

        match tokio::time::timeout_at(
            deadline,
            self.publish_api.publish(&transformed),
        ).await {
            Ok(Ok(())) => MessageResult::Ok,
            _ => MessageResult::Retry,
        }
    }
}
```

`tokio::time::timeout_at` uses an absolute deadline - each call gets the
remaining budget, not a fresh timeout. If enrichment takes 15s, transform
and publish share the remaining 5s.

### Handler processes a batch with chunked API calls

Implement `LeasedHandler` directly for batch/chunked processing. Use
`Batch` for iteration, progress tracking, and remaining lease time:

```rust
use toolkit_db::outbox::{Batch, HandlerResult, LeasedHandler};

#[async_trait::async_trait]
impl LeasedHandler for BulkExportHandler {
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult {
        while !batch.is_empty() {
            let chunk = batch.next_chunk(10);
            if chunk.is_empty() {
                break;
            }

            let mut events = Vec::with_capacity(chunk.len());
            for msg in chunk {
                match serde_json::from_slice::<Event>(&msg.payload) {
                    Ok(e) => events.push(e),
                    Err(e) => {
                        return HandlerResult::Reject {
                            reason: format!("bad payload at seq {}: {e}", msg.seq),
                        };
                    }
                }
            }

            // batch.remaining() returns time until the lease cancel point.
            // The handler owns timeout decisions - the framework only
            // exposes facts (remaining time, message count).
            let timeout = batch.remaining() / 3;

            match tokio::time::timeout(timeout, self.api.bulk_send(&events)).await {
                Ok(Ok(())) => batch.ack_chunk(10),
                Ok(Err(e)) if e.is_transient() => {
                    return HandlerResult::Retry { reason: e.to_string() };
                }
                Ok(Err(e)) => {
                    return HandlerResult::Reject { reason: e.to_string() };
                }
                Err(_) => {
                    return HandlerResult::Retry { reason: "chunk timeout".into() };
                }
            }
        }
        HandlerResult::Success
    }
}
```

Chunk ack is all-or-nothing. If a bulk API partially succeeds, do NOT call
`ack_chunk()` - return `Retry` instead. Idempotency handles the duplicate.

### Handler needs fire-and-forget side work

Use `tokio::spawn` when work must survive handler cancellation (e.g.,
best-effort metrics). The spawned task runs independently - it is NOT
cancelled when the handler future is dropped.

```rust
async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
    let event = match serde_json::from_slice(&msg.payload) {
        Ok(e) => e,
        Err(e) => return MessageResult::Reject(format!("bad payload: {e}")),
    };

    // Main work - cancelled if the handler future is dropped.
    if let Err(e) = self.api.send(&event).await {
        return MessageResult::Retry;
    }

    // Fire-and-forget - survives handler cancellation.
    let metrics = self.metrics.clone();
    tokio::spawn(async move {
        if let Err(e) = metrics.record_delivery(&event).await {
            tracing::warn!(error = %e, "fire-and-forget metric failed");
        }
    });

    MessageResult::Ok
}
```

Use `tokio::spawn` only for best-effort side effects (metrics, logging,
notifications). Avoid it for main business logic - spawned tasks have no
lease guarantee and no backpressure.

### Custom lease for slow handlers

Chain `.lease(LeaseConfig { .. })` after `.leased()`. Defaults: 30s
duration, 2s headroom. The handler cancel point fires at
`duration - headroom`:

```rust
use toolkit_db::outbox::LeaseConfig;

Outbox::builder(db)
    .queue("slow-export", partitions)
    .leased(SlowHandler { api })
    .lease(LeaseConfig {
        duration: Duration::from_secs(300),
        headroom: Duration::from_secs(5),
    })
    .start()
    .await?;
```

The headroom reserves time for the ack DB round-trip after the handler
finishes. It is a fixed cost (not a percentage of the lease).

### Error mapping: HTTP status -> MessageResult

**Do not blanket-reject all 4xx.** A 4xx may come from an intermediate
proxy, API gateway, or service mesh sidecar - not the target service.
Only reject when you are confident the error is permanent and caused by
the message payload itself.

| HTTP status           | `MessageResult` | Rationale                                                                |
|-----------------------|-----------------|--------------------------------------------------------------------------|
| 2xx                   | `Ok`            | Success                                                                  |
| 429                   | `Retry`         | Rate limited - `HttpClient` retries this automatically                   |
| 500 / 502 / 503 / 504 | `Retry`         | Server-side transient error                                              |
| 401 / 403             | `Retry`         | Likely transient - token rotation, IAM propagation delay                 |
| 404                   | `Retry`         | Resource may not be provisioned yet (eventual consistency)               |
| 409                   | `Retry`         | Concurrent write conflict                                                |
| 400                   | `Reject`        | Malformed payload - a bug in the producer, will not self-resolve         |
| 422                   | `Reject`        | Validation failure - the payload content is wrong                        |
| 410                   | `Reject`        | Resource explicitly deleted - no point retrying                          |
| Timeout               | `Retry`         | The call may have succeeded server-side - idempotency handles redelivery |
| Transport error       | `Retry`         | Network issue, connection reset                                          |

**When in doubt, `Retry`.** Dead-lettering is permanent - it removes the
message from processing. A retry that succeeds on the next cycle costs
almost nothing. A false reject loses the message until someone manually
replays it from the dead-letter table.

**`Reject` is for bugs, not for infrastructure.** If the remote service
returns an error because of the message content (bad schema, missing
required field, invalid enum value), that is a `Reject` - the message
will never succeed no matter how many times it is retried. Everything
else is infrastructure noise that will resolve itself.

---

## Benchmarks

### Worker Overhead

Infrastructure overhead (scheduling, notifiers, semaphores) with no-op actions.
A backend feature is still required: the bench declares
`required-features = ["_any-backend"]`, which `sqlite`/`pg`/`mysql` activate.

```bash
cargo bench -p cf-gears-toolkit-db --features sqlite --bench worker_overhead
```

### Outbox Throughput

End-to-end throughput with per-partition ordering verification.
Requires a database feature flag:

```bash
# SQLite (local, no external DB needed)
cargo bench -p cf-gears-toolkit-db --features sqlite --bench outbox_throughput

# PostgreSQL
cargo bench -p cf-gears-toolkit-db --features pg --bench outbox_throughput -- postgres

# MySQL
cargo bench -p cf-gears-toolkit-db --features mysql --bench outbox_throughput -- mysql
```

### Makefile Targets

```bash
make bench-pg              # PostgreSQL standard
make bench-pg-longhaul     # PostgreSQL 1M + 10M messages
make bench-mysql           # MySQL standard
make bench-sqlite          # SQLite standard
make bench-db              # All engines
make bench-db-longhaul     # All engines, long-haul
```

### Resource-Limited Runs

```bash
systemd-run --user --scope -p MemoryMax=4G -p CPUQuota=200% \
  cargo bench -p cf-gears-toolkit-db --features sqlite --bench worker_overhead \
  -- --warm-up-time 1 --measurement-time 3 --sample-size 10
```
