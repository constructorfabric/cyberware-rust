//! Transactional outbox for reliable asynchronous message production.
//!
//! # Architecture
//!
//! Four-stage pipeline: **incoming → sequencer → outgoing → processor**.
//!
//! 1. **Enqueue** - messages are written atomically within business transactions
//!    to the `incoming` table via [`Outbox::enqueue()`].
//! 2. **Sequencer** - a background task claims incoming rows, assigns
//!    per-partition sequence numbers, and writes to the `outgoing` table.
//! 3. **Processor** - one long-lived task per partition reads from `outgoing`,
//!    dispatches to the registered handler, and acks via cursor advance
//!    (append-only - no deletes on the hot path).
//! 4. **Vacuum** - a standalone background task (peer of the sequencer) that
//!    garbage-collects processed outgoing and body rows across dirty partitions.
//!
//! # Traces
//!
//! A batch may be submitted under a caller-supplied **trace**, and the
//! instance that submitted it is told once every entity in the batch has
//! reached a terminal state, and can watch it for stalls while it is in
//! flight - see [`Outbox::subscribe`], [`TraceSubscription::progress`] and
//! [`Outbox::trace_status`]. It is opt-in: a submission with no trace records
//! nothing.
//!
//! # Why so many tasks
//!
//! Each task does **one simple thing** and then conditionally tells another
//! that there is something to do. New work belongs in a new task rather than
//! appended to an existing one.
//!
//! A task that does A and then B couples them twice over: B inherits A's
//! failure modes, and B inherits A's pacing. If A fails intermittently - a
//! contended delete, a lock it could not take - B silently stops running, and
//! nobody notices because A's own error was logged and retried. And if A is
//! configured to run hourly because that suits A, B runs hourly too, for no
//! reason of its own.
//!
//! So the pipeline is a fleet of small tasks and many channels between them:
//!
//! | task | does one thing | tells |
//! |---|---|---|
//! | sequencer | claims incoming rows, assigns sequence numbers, writes outgoing | the partition's processor |
//! | processor | reads a batch, runs the handler, acks | - |
//! | vacuum | deletes processed outgoing and body rows | the trace sweeper, when it deleted any |
//! | trace sweeper | collects finished trace rows | - |
//! | cold reconciler | rediscovers pending partitions from the incoming table | the sequencer |
//! | notifier | collects this instance's completion mail | the caller waiting on it |
//! | stall reporter | reports this instance's stuck batches | the caller watching one |
//!
//! The telling is **conditional**, and the bar is not "something happened" but
//! "the receiver could not otherwise find out, and has a reason to act". The
//! vacuum tells the sweeper only when it actually deleted bodies - the one
//! thing that can make a trace collectable ahead of its own clock - rather
//! than after every sweep. A task that was not signalled falls back to a slow
//! poll or does not run at all. That is what makes many small tasks cheaper
//! than a few large ones rather than dearer - the notifier issues no query
//! while nothing is outstanding.
//!
//! Channels are [`taskward::poker`] for a timer, a `Notify` per partition
//! where the listener's identity *is* the subject, and a bare `Notify` where
//! the wakeup says nothing. Adding another is the expected move. The
//! prioritizer is not one of these: it decides *which partition next* for the
//! sequencers, which is work distribution rather than delivery.
//!
//! One task is honestly not this shape: the **processor** reads, hands the
//! batch to the handler, and acks, because the lease it holds spans all three
//! - splitting them would mean handing the lease across a channel.
//!
//! # Who writes what
//!
//! Small tasks are only decoupled if they are decoupled in the database too,
//! and the unit of ownership is the **table**: one table, one piece of
//! functionality. Not one writer - the trace table is written by the enqueue,
//! the ack, the owner's notifier and the sweep, and that is fine because those
//! are all the trace's own lifecycle. What must not share a table is unrelated
//! functionality, because then nobody can say when a row is locked or by whom,
//! and how coarse that lock gets is not the same on every backend.
//!
//! | table | the functionality that owns it | written by |
//! |---|---|---|
//! | incoming | work waiting to be sequenced | the enqueue inserts, the sequencer deletes |
//! | body | the payloads | the enqueue inserts, the vacuum deletes |
//! | outgoing | work waiting to be processed | the sequencer inserts, the vacuum deletes |
//! | partitions | sequence allocation | the sequencer alone |
//! | processor | the cursor and the lease | the ack alone |
//! | vacuum counter | telling the vacuum there is something to collect | the ack bumps it, the vacuum decrements it |
//! | trace | one traced batch's lifetime | the enqueue inserts, the ack counts down, the owner claims, the sweep deletes |
//! | dead letters | entities that failed for good | the ack inserts, the dead-letter API resolves and deletes |
//!
//! An ack writes four of those in one transaction, and that is not a violation
//! of the rule: a terminal state is one fact, and it has to land atomically -
//! the cursor moves, the vacuum is told there is something to collect, the
//! trace it belonged to counts down, and a rejected entity becomes a dead
//! letter. Four tables, one commit, one fact.
//!
//! Reading another stage's table is unremarkable and several tasks do it - the
//! notifier and the stall reporter read the trace table that the ack writes.
//!
//! # Processing modes
//!
//! - **Transactional** - handler runs inside the DB transaction holding the
//!   partition lock. Provides exactly-once semantics within the database.
//! - **Leased** - handler runs outside any transaction, with lease-based
//!   locking. Provides at-least-once delivery; handlers must be idempotent.
//!
//! # Usage
//!
//! ```ignore
//! run_migrations_for_testing(&db, outbox_migrations()).await?;
//!
//! let handle = Outbox::builder(db)
//!     .profile(OutboxProfile::low_latency())
//!     .queue("orders", Partitions::of(4))
//!         .leased(my_handler)
//!     .start().await?;
//! // ... enqueue via handle.outbox() ...
//! handle.stop().await;
//! ```
//!
//! Custom table prefixes are supported when migrations and runtime use the
//! same validated prefix:
//!
//! ```ignore
//! run_migrations_for_testing(&db, outbox_migrations_with_prefix("mini_chat_outbox")?).await?;
//!
//! let handle = Outbox::builder(db)
//!     .table_prefix("mini_chat_outbox")?
//!     .queue("orders", Partitions::of(4))
//!         .leased(my_handler)
//!     .start().await?;
//! ```
//!
//! Prefix changes create a new outbox table family; they do not rename or move
//! existing rows. Prefixes are validated as portable SQL identifiers because
//! table and index names cannot be bound as SQL parameters.
//! At runtime the validated table names and detected database backend are
//! compiled once into an internal statement catalog; fixed SQL is not produced
//! by repeated default-table-name replacement.
//!
//! # Backend notes
//!
//! - **`PostgreSQL`** - Full support. Uses `FOR UPDATE SKIP LOCKED` for partition
//!   locking and `INSERT ... RETURNING` for body ID retrieval.
//! - **`MySQL` 8.0+** - Requires `MySQL` 8.0 or later for `FOR UPDATE SKIP LOCKED`
//!   support (added in 8.0.1). Earlier versions will fail at runtime when
//!   attempting to acquire partition locks. Batch enqueue reserves explicit IDs
//!   from `<prefix>_body_id_sequence` and `<prefix>_incoming_id_sequence` in a
//!   fixed order; retry the whole transaction on deadlock or cluster
//!   certification failures.
//! - **`SQLite`** - Single-process only. `SQLite` has no row-level locking; the
//!   outbox relies on `SQLite`'s single-writer model. Suitable for development,
//!   testing, and single-instance deployments. Not recommended for production
//!   multi-process scenarios.
//!
//! # Dead letters
//!
//! Messages that a handler permanently rejects ([`HandlerResult::Reject`]) are
//! moved to a dead-letter table with the original payload, partition, sequence,
//! and error reason preserved. The outbox does **not** auto-replay dead letters;
//! that policy is owned by the application.
//!
//! Dead letter operations are available as methods on [`Outbox`]:
//! [`dead_letter_list`](Outbox::dead_letter_list),
//! [`dead_letter_count`](Outbox::dead_letter_count),
//! [`dead_letter_replay`](Outbox::dead_letter_replay),
//! [`dead_letter_resolve`](Outbox::dead_letter_resolve),
//! [`dead_letter_reject`](Outbox::dead_letter_reject),
//! [`dead_letter_discard`](Outbox::dead_letter_discard), and
//! [`dead_letter_cleanup`](Outbox::dead_letter_cleanup).
//!
//! Dead letters have a status lifecycle: `pending → reprocessing → resolved`
//! (or `pending → discarded`). The [`DeadLetterStatus`] enum tracks this.
//!
//! ## Example: application-level consumption
//!
//! The library provides the building blocks; the application decides **when**
//! and **how** to use them. `dead_letter_replay` claims messages (sets them
//! to `reprocessing` with a deadline) and returns them - the application
//! then processes and calls `resolve` or `reject`.
//!
//! ```ignore
//! use std::time::Duration;
//!
//! let scope = DeadLetterScope::default().payload_type("order.created");
//! let msgs = outbox.dead_letter_replay(&conn, &scope, Duration::from_secs(60)).await?;
//! for msg in &msgs {
//!     match my_handler(&msg.payload).await {
//!         Ok(_)  => outbox.dead_letter_resolve(&conn, &[msg.id]).await?,
//!         Err(e) => outbox.dead_letter_reject(&conn, &[msg.id], &e.to_string()).await?,
//!     };
//! }
//! ```

mod batch;
mod builder;
mod core;
mod dead_letter;
mod dialect;
mod handler;
mod manager;
mod migrations;
pub(crate) mod prioritizer;
mod record;
mod statements;
pub(crate) mod stats;
mod store;
mod strategy;
mod subscription;
mod tables;
#[doc(hidden)]
pub mod taskward;
mod trace;
mod types;
pub(crate) mod validation;
mod workers;

#[cfg(test)]
#[cfg(feature = "sqlite")]
#[cfg_attr(coverage_nightly, coverage(off))]
mod integration_tests;

pub use batch::Batch;
pub use builder::{LeasedQueueBuilder, QueueBuilder};
pub use core::Outbox;
pub use dead_letter::{DeadLetterFilter, DeadLetterMessage, DeadLetterScope, DeadLetterStatus};
pub use handler::{
    HandlerResult, LeasedHandler, LeasedMessageHandler, MessageResult, OutboxMessage,
    PerMessageAdapter, TransactionalHandler, TransactionalMessageHandler,
};
pub use manager::{OutboxBuilder, OutboxHandle};
pub use migrations::{outbox_migrations, outbox_migrations_with_prefix};
pub use record::{Record, RecordBuilder, RecordTarget, Records, RecordsBuilder, RecordsTarget};
pub use subscription::{TraceProgressWatch, TraceSubscription};
pub use trace::{TraceAdvance, TraceOutcome, TraceProgress, TraceStatus};
pub use types::{
    LeaseConfig, OutboxError, OutboxMessageId, OutboxProfile, Partitions, WorkerTuning,
};

// Internal re-exports for tests and internal gears
