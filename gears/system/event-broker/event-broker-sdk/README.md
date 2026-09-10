# cf-gears-event-broker-sdk

High-level Rust SDK for the `event-broker` gear.

Wire concerns (JSON serialisation, partition selection, producer-chain bookkeeping,
subscription lifecycle recovery, canonical error handling) are handled inside the
SDK. Callers work with their own typed event structs, a single `EventBrokerApi` trait,
and structured `Producer` / `Consumer` builders.

## Error model

The SDK traits and wrappers use domain-readable local errors in-process:
`EventBrokerApi` returns `EventBrokerError`, `EventBrokerBackend` returns
`StorageBackendError`, and producer/consumer wrappers return `EventBrokerError`
for local typed validation, retry, and dispatch. Those errors are canonical-error
compatible: before errors cross an API or transport boundary they convert to the
canonical categories and RFC 9457 `Problem` representation from
`toolkit-canonical-errors`.

`Problem.instance` and `Problem.trace_id` are boundary-owned fields. SDK domain
errors do not treat them as business data; HTTP or RPC middleware attaches them
when converting a `CanonicalError` into the final problem response.

## Design source

Sourced from `modules/system/event-broker/docs/DESIGN.md` on the `event-broker-design`
branch.

```
DESIGN_PIN = bb8e169ee04eb40bdda18ff3a01da980c86fa546
```

---

## Producer quick-start

### DB-free direct producer

```rust
use std::borrow::Cow;
use std::sync::Arc;

use event_broker_sdk::{
    DirectDeduplication, EventBrokerApi, Producer, ProducerIdentity, TypedEvent,
};

#[derive(Serialize, Deserialize)]
struct OrderCreated { order_id: Uuid, total_cents: i64 }

impl TypedEvent for OrderCreated {
    const TYPE_ID: &'static str = "gts.cf.core.events.event.v1~orders.created.v1~";
    const SUBJECT_TYPE: &'static str = "gts.cf.core.events.subject.v1~order.v1";
    const SOURCE:  &'static str = "order-service";
    fn subject(&self) -> Cow<'_, str> { Cow::Owned(self.order_id.to_string()) }
}

// Obtain from ClientHub.
let broker = hub.get::<dyn EventBrokerApi>()?;

let producer = Producer::builder()
    .broker(Arc::clone(&broker))
    .security_context(ctx.clone())
    .identity(ProducerIdentity::new().source("order-service"))
    .deduplication(DirectDeduplication::stateless())
    .topics(["gts.cf.core.events.topic.v1~orders.v1"])
    .event_type_patterns(["gts.cf.core.events.event.v1~orders.*"])
    .prepare_all()
    .await?;

producer.publish(OrderCreated { order_id, total_cents: 4299 }).await?;
producer.publish_persisted(OrderCreated { order_id, total_cents: 4299 }).await?;
```

### DB-aware managed outbox producer (`outbox` feature)

```rust
use event_broker_sdk::{
    DbDeduplication, DbProducer, MissingProducerRegistration, ProducerIdentity,
    ProducerMode, UnknownProducerRegistration, producer_registration_migrations,
};
use toolkit_db::outbox::{Outbox, Partitions};

// Run explicitly during service migration, before constructing DbProducer.
toolkit_db::migration_runner::run_migrations_for_gear(
    &db,
    "event-broker-producer",
    producer_registration_migrations(),
)
.await?;

let producer = DbProducer::builder()
    .broker(Arc::clone(&broker))
    .db(db.clone())
    .security_context(ctx.clone())
    .identity(
        ProducerIdentity::new()
            .source("order-service")
            .client_agent("order-service/1.0"),
    )
    .deduplication(
        DbDeduplication::managed(ProducerMode::Chained)
            .key("orders.created")
            .on_missing(MissingProducerRegistration::RegisterNew)
            .on_unknown(UnknownProducerRegistration::RegisterNew),
    )
    .topics(["gts.cf.core.events.topic.v1~orders.v1"])
    .event_type_patterns(["gts.cf.core.events.event.v1~orders.*"])
    .prepare_all()
    .await?;

let event_outbox = producer.outbox_queue("event-broker-producer", Partitions::of(16))?;
let outbox_handle = event_outbox
    .register(Outbox::builder(db.clone()))
    .start()
    .await?;
let producer_outbox = event_outbox.bind(&outbox_handle);

let mut txn = db.begin().await?;
write_business_state(&txn).await?;
producer_outbox.enqueue(&txn, OrderCreated { ... }).await?;
txn.commit().await?;
```

`DbProducer` validates typed events before writing producer outbox rows. In lazy
validation mode, call `producer.prepare::<OrderCreated>().await?` or
`producer.prepare_all().await?` before opening the business transaction; enqueue
does not call Event Broker while the transaction is open.

### Producer configuration matrix

| Surface | Deduplication | Local DB | Producer id | Multi-instance notes |
|---|---|---|---|
| `Producer` | `DirectDeduplication::stateless()` | No | None | Safe for horizontally scaled producers when consumers are idempotent |
| `Producer` | `register_on_start(mode)` | No | Fresh broker-issued id per process | Deduplication identity resets on process restart |
| `Producer` | `reuse(mode, producer_id)` | No | Caller-supplied broker-issued id | Caller must provide durable id storage and single-writer coordination |
| `DbProducer` | `DbDeduplication::stateless()` | Yes | None | Uses DB only for producer surface/outbox integration, not producer id |
| `DbProducer` | `managed(mode).key(...)` | Yes | Loaded or registered broker-issued id | Preferred durable chained/monotonic outbox path |

Producer ids are always broker-issued. The SDK does not mint producer ids and
does not persist a separate last-sent-sequence table. Direct non-stateless
producers rebuild in-memory sequence state from Event Broker cursors on start.
Outbox producers use toolkit-db `OutboxMessage.seq` as the durable local
sequence and Event Broker cursors as the authoritative accepted sequence.

One `ProducerOutboxQueue` can carry all topics configured on a `DbProducer`.
The SDK maps `(topic, broker_partition)` to one producer outbox partition, so
topic partition counts and outbox queue partition counts can differ without
breaking ordering for a topic partition.

The service owns toolkit-db outbox lifecycle: run toolkit-db outbox migrations,
register queues, start workers, tune leases, and stop the `OutboxHandle`.
`ProducerOutboxQueue` only binds the SDK producer queue and leased processor to
the service-provided builder.

---

## Consumer quick-start

Consumers use a **typestate builder** for commit mode:

| Builder state | Outcome enum | Commit handle | Use when |
|---|---|---|---|
| `offset_manager(InMemoryOffsetManager)` | `HandlerOutcome` or `BatchHandlerOutcome` | None | Simple, in-process cursor |
| `offset_manager(custom CommitOffset)` | `HandlerOutcome` or `BatchHandlerOutcome` | None | Remote or custom cursor |
| `offset_manager(LocalDbOffsetManager)` | `HandlerOutcome` | `TxCommitHandle<LocalDbOffsetManager>` | Atomic DB cursor |

Dead-letter behavior is handler-owned policy. If a permanent failure should be parked, the
handler writes it through `event_broker_sdk::dlq` helpers or application code, then returns
`Success` only after the parking operation is durable. If parking fails, return retry or an
error so the source offset does not advance.

### Single-event consumer

```rust
use event_broker_sdk::{
    ConsumerBuilder, ConsumerError, ConsumerGroupRef, ConsumerProfile, EventTypeRef,
    Fallback, HandlerOutcome, InMemoryOffsetManager, RawEvent, SingleEventHandler,
    SubscriptionInterest, TopicRef,
};

struct BillingProjector;

#[async_trait]
impl SingleEventHandler for BillingProjector {
    async fn handle(&self, event: RawEvent, attempts: u16)
        -> Result<HandlerOutcome, ConsumerError>
    {
        // match on event.type_id and process
        Ok(HandlerOutcome::Success)
    }
}

let handle = ConsumerBuilder::new(broker.clone())
    .group(ConsumerGroupRef::auto_anonymous("billing-projector"))
    .subscription_interests([SubscriptionInterest::builder()
        .topic(TopicRef::gts("gts.cf.core.events.topic.v1~orders.v1"))
        .types([EventTypeRef::gts_pattern("gts.cf.core.events.event.v1~orders.*")])
        .build()?])
    .profile(ConsumerProfile::low_latency())
    .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
    .handler(BillingProjector)
    .start()
    .await?;

handle.stop().await?;
```

### Batch consumer

```rust
use event_broker_sdk::{BatchHandlerOutcome, ConsumerBatching, ConsumerBuilder, ConsumerError, ConsumerHandler, EventBatch};
use std::time::Duration;

struct BatchProjector;

#[async_trait]
impl ConsumerHandler for BatchProjector {
    async fn handle_batch(&self, batch: &EventBatch<'_>, attempts: u16)
        -> Result<BatchHandlerOutcome, ConsumerError>
    {
        let chunk = batch.next_chunk(batch.len());
        for event in chunk {
            // process events from one topic partition
        }
        Ok(BatchHandlerOutcome::AdvanceThrough {
            offset: chunk.last().expect("non-empty batch").offset,
        })
    }
}

let handle = ConsumerBuilder::new(broker.clone())
    .group(ConsumerGroupRef::auto_anonymous("billing-batch"))
    .subscription_interests([orders_interest])
    .batching(ConsumerBatching { max_events: 128, max_wait: Duration::from_millis(250) })
    .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
    .batch_handler(BatchProjector)
    .start()
    .await?;
```

### Routed handlers

```rust
let handle = ConsumerBuilder::new(broker.clone())
    .group(ConsumerGroupRef::auto_anonymous("commerce-router"))
    .subscription_interests([orders_interest])
    .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
    .default_handler(DefaultProjector)
    .route()
    .topic(TopicRef::gts("gts.cf.core.events.topic.v1~orders.v1"))
    .event_type(EventTypeRef::gts("gts.cf.core.events.event.v1~orders.created.v1~"))
    .handler(OrderCreatedProjector)
    .start()
    .await?;
```

### Consumer with DB cursor and outbox-backed DLQ (`outbox` feature)

```rust
use std::sync::Arc;

use event_broker_sdk::dlq::{ConsumerDlqOutbox, DeadLetterRecord};
use event_broker_sdk::{
    ConsumerBuilder, ConsumerError, Fallback, HandlerOutcome, LocalDbOffsetManager, RawEvent,
    TxCommitHandle, TxSingleEventHandler,
};

#[async_trait]
impl TxSingleEventHandler<LocalDbOffsetManager> for MyHandler {
    async fn handle(
        &self,
        event: RawEvent,
        attempts: u16,
        commit: TxCommitHandle<LocalDbOffsetManager>,
    )
        -> Result<HandlerOutcome, ConsumerError>
    {
        if attempts > 5 {
            let record = DeadLetterRecord::builder(&event, "too many retries")
                .attempts(attempts)
                .build();

            self.db.transaction_ref(|tx| {
                Box::pin(async move {
                    self.dlq.enqueue(tx, record).await?;
                    commit.commit_offset_in_tx(tx, event.offset).await?;
                    Ok(())
                })
            }).await?;

            return Ok(HandlerOutcome::Success);
        }

        self.db.transaction_ref(|tx| {
            Box::pin(async move {
                self.project(tx, &event).await?;
                commit.commit_offset_in_tx(tx, event.offset).await?; // offset + business state atomic
                Ok(())
            })
        }).await?;

        Ok(HandlerOutcome::Success)
    }
}

// This starts the service-owned DLQ outbox queue. The SDK helper only enqueues
// a durable handoff record; your outbox processor owns final DLQ delivery.
let outbox_handle = toolkit_db::outbox::Outbox::builder(db.clone())
    .queue("consumer-dlq", toolkit_db::outbox::Partitions::of(4))
    .leased(MyDlqProcessor)
    .start()
    .await?;

let dlq = ConsumerDlqOutbox::builder(Arc::clone(outbox_handle.outbox()))
    .queue("consumer-dlq")
    .partitions(4)
    .build();

let handle = ConsumerBuilder::new(broker.clone())
    .group(...)
    .subscription_interests([...])
    .offset_manager(LocalDbOffsetManager::new(db.clone(), Fallback::Earliest))
    .handler(MyHandler { db, dlq })
    .start()
    .await?;
```

If the main business transaction already rolled back, open a new transaction for
the DLQ handoff and offset skip. If that DLQ transaction fails, return an error
from the handler so the source offset is not advanced. Services that need a
custom table, remote sink, or in-memory parking can still implement
`DeadLetterSink` directly.

---

## Features

| Feature | Enables | Extra deps |
|---|---|---|
| (default) | DB-free direct producer, consumer | — |
| `db` | `LocalDbOffsetManager`, `TxCommitHandle` | `toolkit-db` |
| `outbox` | DB-aware producer outbox helper | `db` |
| `integration` | Integration tests (gated) | — |
