use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::core::Outbox;
use super::handler::{
    LeasedHandler, PerMessageAdapter, TransactionalHandler, TransactionalMessageHandler,
};
use super::manager::{OutboxBuilder, QueueDeclaration};
use super::stats::StatsRegistry;
use super::strategy::{LeasedStrategy, TransactionalStrategy, generate_worker_id};
use super::taskward::{
    BackoffConfig, Bulkhead, BulkheadConfig, ConcurrencyLimit, PanicPolicy, TracingListener,
    WorkerBuilder,
};
use super::types::LeaseConfig;
use super::types::{Partitions, WorkerTuning};
use super::workers::processor::{PartitionProcessor, ProcessorReport};
use crate::Db;

/// All runtime context needed to spawn a processor worker.
/// Constructed once per partition in [`OutboxBuilder::start()`].
pub struct SpawnContext {
    pub pid: i64,
    pub db: Db,
    pub cancel: CancellationToken,
    pub partition_notify: Arc<Notify>,
    pub processor_sem: Arc<Semaphore>,
    pub start_notify: Arc<Notify>,
    #[allow(dead_code)]
    pub outbox: Arc<Outbox>,
    /// Shared stats registry for processor workers. `None` when stats disabled.
    pub stats_registry: Option<Arc<std::sync::Mutex<StatsRegistry>>>,
    /// Processor worker tuning (batch size, pacing, retry backoff).
    pub tuning: WorkerTuning,
}

/// Trait for creating processor workers. One impl per processing mode.
pub trait ProcessorFactory: Send {
    fn spawn(&self, ctx: SpawnContext) -> (String, Pin<Box<dyn Future<Output = ()> + Send>>);
}

/// Shared worker assembly logic for all processor factories.
fn build_processor_worker<S: super::strategy::ProcessingStrategy + 'static>(
    ctx: &SpawnContext,
    strategy: S,
) -> (String, Pin<Box<dyn Future<Output = ()> + Send>>) {
    let processor = PartitionProcessor::new(
        strategy,
        ctx.pid,
        ctx.tuning.clone(),
        ctx.db.clone(),
        &ctx.outbox,
    );
    let name = format!("processor-{}", ctx.pid);
    let (poker_notify, _poker_handle) =
        super::taskward::poker(ctx.tuning.idle_interval, ctx.cancel.clone());
    let mut builder = WorkerBuilder::<ProcessorReport>::new(&name, ctx.cancel.clone())
        .pacing(&ctx.tuning)
        .notifier(poker_notify)
        .notifier(Arc::clone(&ctx.partition_notify))
        .notifier(Arc::clone(&ctx.start_notify))
        .bulkhead(Bulkhead::new(
            &name,
            BulkheadConfig {
                semaphore: ConcurrencyLimit::Fixed(Arc::clone(&ctx.processor_sem)),
                backoff: BackoffConfig::default(),
            },
        ))
        .listener(TracingListener)
        .on_panic(PanicPolicy::CatchAndRetry);

    builder = super::manager::register_stats(
        builder,
        ctx.stats_registry.as_ref(),
        "processor",
        Box::new(|any| {
            any.downcast_ref::<ProcessorReport>()
                .map_or(0, |r| u64::from(r.messages_processed))
        }),
    );

    let worker = builder.build(processor);
    (name, Box::pin(worker.run()))
}

/// Builder for registering a queue with per-queue configuration.
///
/// Obtained via [`OutboxBuilder::queue`]. Terminal methods (`.leased()`,
/// `.transactional()`, `.batch_transactional()`) register the queue and
/// return the parent builder for chaining.
#[must_use = "a queue builder does nothing until a handler is registered via .leased() or .transactional()"]
pub struct QueueBuilder {
    builder: OutboxBuilder,
    name: String,
    partitions: Partitions,
}

impl QueueBuilder {
    pub(crate) fn new(builder: OutboxBuilder, name: String, partitions: Partitions) -> Self {
        Self {
            builder,
            name,
            partitions,
        }
    }

    /// Register a single-message transactional handler (common case).
    ///
    /// The `PerMessageAdapter` processes messages one at a time inside the
    /// transaction, tracking partial progress via `processed_count()`.
    #[must_use]
    pub fn transactional(
        self,
        handler: impl TransactionalMessageHandler + 'static,
    ) -> OutboxBuilder {
        self.register_transactional(PerMessageAdapter::new(handler))
    }

    /// Register a batch transactional handler (advanced).
    #[must_use]
    pub fn batch_transactional(
        self,
        handler: impl TransactionalHandler + 'static,
    ) -> OutboxBuilder {
        self.register_transactional(handler)
    }

    fn register_transactional(self, handler: impl TransactionalHandler + 'static) -> OutboxBuilder {
        let factory = TransactionalProcessorFactory {
            handler: Arc::new(handler),
        };

        let mut builder = self.builder;
        builder.queue_declarations.push(QueueDeclaration {
            name: self.name,
            partitions: self.partitions,
            factory: Box::new(factory),
        });
        builder
    }

    /// Register a leased handler.
    ///
    /// Accepts any `LeasedHandler` (batch) or `LeasedMessageHandler` (per-message,
    /// via blanket impl). The framework enforces lease-aware cancellation by
    /// dropping the handler future - no `CancellationToken` is passed.
    ///
    /// Chain `.lease(LeaseConfig { .. })` after this to customize lease duration
    /// and ack headroom. Defaults: 30s duration, 2s headroom.
    pub fn leased(self, handler: impl LeasedHandler + 'static) -> LeasedQueueBuilder {
        let factory = LeasedProcessorFactory {
            handler: Arc::new(handler),
            queue_name: self.name.clone(),
            lease_config: LeaseConfig::default(),
        };

        LeasedQueueBuilder {
            builder: self.builder,
            name: self.name,
            partitions: self.partitions,
            factory,
        }
    }
}

/// Builder state after `.leased()` - allows optional `.lease()` config.
#[must_use = "call .queue() or .start() to finalize"]
pub struct LeasedQueueBuilder {
    builder: OutboxBuilder,
    name: String,
    partitions: Partitions,
    factory: LeasedProcessorFactory,
}

impl LeasedQueueBuilder {
    /// Customize lease duration and ack headroom for this queue.
    pub fn lease(mut self, config: LeaseConfig) -> Self {
        config.validate();
        self.factory.lease_config = config;
        self
    }

    /// Start a new queue definition (finalizes the current one).
    pub fn queue(self, name: &str, partitions: Partitions) -> QueueBuilder {
        self.done().queue(name, partitions)
    }

    /// Start the outbox (finalizes the current queue).
    ///
    /// # Errors
    ///
    /// Returns `OutboxError` if the outbox fails to start (e.g., migration or
    /// connection issues).
    pub async fn start(self) -> Result<super::manager::OutboxHandle, super::types::OutboxError> {
        self.done().start().await
    }

    /// Finalize the current queue and return the parent builder.
    /// Use when you need the `OutboxBuilder` back (e.g., in a loop).
    #[must_use]
    pub fn done(self) -> OutboxBuilder {
        let mut builder = self.builder;
        builder.queue_declarations.push(QueueDeclaration {
            name: self.name,
            partitions: self.partitions,
            factory: Box::new(self.factory),
        });
        builder
    }
}

// --- Factory implementations ---

struct TransactionalProcessorFactory<H: TransactionalHandler> {
    handler: Arc<H>,
}

impl<H: TransactionalHandler + 'static> ProcessorFactory for TransactionalProcessorFactory<H> {
    fn spawn(&self, ctx: SpawnContext) -> (String, Pin<Box<dyn Future<Output = ()> + Send>>) {
        let strategy = TransactionalStrategy::new(Box::new(ArcTransactionalHandler(Arc::clone(
            &self.handler,
        ))));
        build_processor_worker(&ctx, strategy)
    }
}

/// Delegates `TransactionalHandler` to the inner `Arc<H>` so a single
/// handler instance can be shared across partition processor workers.
struct ArcTransactionalHandler<H: TransactionalHandler>(Arc<H>);

#[async_trait::async_trait]
impl<H: TransactionalHandler> TransactionalHandler for ArcTransactionalHandler<H> {
    async fn handle(
        &self,
        txn: &sea_orm::DatabaseExecutor<'_>,
        msgs: &[super::handler::OutboxMessage],
    ) -> super::handler::HandlerResult {
        self.0.handle(txn, msgs).await
    }

    fn processed_count(&self) -> Option<usize> {
        self.0.processed_count()
    }
}

struct LeasedProcessorFactory {
    handler: Arc<dyn LeasedHandler>,
    queue_name: String,
    lease_config: LeaseConfig,
}

impl ProcessorFactory for LeasedProcessorFactory {
    fn spawn(&self, mut ctx: SpawnContext) -> (String, Pin<Box<dyn Future<Output = ()> + Send>>) {
        ctx.tuning.lease_duration = self.lease_config.duration;
        let worker_id = generate_worker_id(&self.queue_name);
        let strategy = LeasedStrategy::new(Arc::clone(&self.handler), worker_id, self.lease_config);
        build_processor_worker(&ctx, strategy)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn partitions_count() {
        assert_eq!(Partitions::of(1).count(), 1);
        assert_eq!(Partitions::of(2).count(), 2);
        assert_eq!(Partitions::of(4).count(), 4);
        assert_eq!(Partitions::of(8).count(), 8);
        assert_eq!(Partitions::of(16).count(), 16);
        assert_eq!(Partitions::of(32).count(), 32);
        assert_eq!(Partitions::of(64).count(), 64);
    }
}
