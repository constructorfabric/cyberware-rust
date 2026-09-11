use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::builder::QueueBuilder;
use super::core::Outbox;
use super::prioritizer::SharedPrioritizer;
use super::stats::{StatsListener, StatsRegistry, StatsReporter};
use super::tables::OutboxTables;
use super::taskward::{
    BackoffConfig, Bulkhead, BulkheadConfig, ConcurrencyLimit, PanicPolicy, TaskSet,
    TracingListener, WorkerBuilder, poker,
};
use super::types::{
    OutboxConfig, OutboxError, OutboxProfile, Partitions, SequencerConfig, WorkerTuning,
};
use super::workers::sequencer::{Sequencer, SequencerReport};
use super::workers::vacuum::{VacuumReport, VacuumTask};
use crate::Db;

/// Deferred queue declaration — factory, resolved at `start()`.
pub struct QueueDeclaration {
    pub(crate) name: String,
    pub(crate) partitions: Partitions,
    pub(crate) factory: Box<dyn super::builder::ProcessorFactory>,
}

// ---------------------------------------------------------------------------
// Internal helpers for start() decomposition
// ---------------------------------------------------------------------------

/// Resolved per-worker tuning values (per-worker > profile > hardcoded default).
struct ResolvedTuning {
    processor: WorkerTuning,
    sequencer: WorkerTuning,
    vacuum: WorkerTuning,
    reconciler: WorkerTuning,
    notifier: WorkerTuning,
    trace_sweeper: WorkerTuning,
}

/// Short-lived context bag passed to spawn helpers during `start()`.
struct StartContext<'a> {
    db: &'a Db,
    cancel: &'a CancellationToken,
    task_set: &'a mut super::taskward::TaskSet,
    start_notify: &'a Arc<Notify>,
    stats_registry: &'a Option<Arc<std::sync::Mutex<super::stats::StatsRegistry>>>,
}

/// Wire a [`StatsListener`] into a [`WorkerBuilder`] if stats collection is enabled.
///
/// Eliminates the duplicated stats-wiring pattern used by sequencer, vacuum, and
/// processor worker factories.
type StatsExtractor = Box<dyn Fn(&dyn std::any::Any) -> u64 + Send + Sync>;

pub(super) fn register_stats<P: Send + Sync + 'static>(
    builder: WorkerBuilder<P>,
    registry: Option<&Arc<std::sync::Mutex<StatsRegistry>>>,
    category: &str,
    extractor: StatsExtractor,
) -> WorkerBuilder<P> {
    if let Some(reg) = registry {
        let stats = StatsListener::new(extractor);
        if let Ok(mut guard) = reg.lock() {
            guard.register(category.to_owned(), stats.clone());
        }
        builder.listener(stats)
    } else {
        builder
    }
}

/// Fluent builder for the outbox pipeline.
///
/// Entry point: [`Outbox::builder(db)`](Outbox::builder). Configure global
/// settings and register queues with handlers, then call
/// [`start()`](Self::start) to spawn background tasks.
///
/// # Tuning API
///
/// Use [`profile()`](Self::profile) to set a baseline tuning profile for all
/// workers, then optionally override individual workers with
/// [`processor_tuning()`](Self::processor_tuning),
/// [`sequencer_tuning()`](Self::sequencer_tuning),
/// [`vacuum_tuning()`](Self::vacuum_tuning), or
/// [`reconciler_tuning()`](Self::reconciler_tuning).
///
/// Resolution order: per-worker tuning > profile > hardcoded default.
///
/// ```ignore
/// let handle = Outbox::builder(db)
///     .profile(OutboxProfile::high_throughput())
///     .processor_tuning(WorkerTuning::processor_high_throughput().batch_size(50))
///     .queue("orders", Partitions::of(4))
///         .leased(my_handler)
///     .start().await?;
/// // enqueue via handle.outbox()
/// handle.stop().await;
/// ```
pub struct OutboxBuilder {
    db: Db,
    partition_batch_limit: u32,
    max_inner_iterations: u32,
    processors: Option<usize>,
    maintenance_guaranteed: Option<usize>,
    maintenance_shared: Option<usize>,
    stats_interval: Option<Duration>,
    profile: Option<OutboxProfile>,
    processor_tuning: Option<WorkerTuning>,
    sequencer_tuning: Option<WorkerTuning>,
    vacuum_tuning: Option<WorkerTuning>,
    reconciler_tuning: Option<WorkerTuning>,
    tables: OutboxTables,
    pub(crate) queue_declarations: Vec<QueueDeclaration>,
}

impl OutboxBuilder {
    pub(crate) fn new(db: Db) -> Self {
        Self {
            db,
            partition_batch_limit: super::types::DEFAULT_PARTITION_BATCH_LIMIT,
            max_inner_iterations: super::types::DEFAULT_MAX_INNER_ITERATIONS,
            processors: None,
            maintenance_guaranteed: None,
            maintenance_shared: None,
            stats_interval: Some(Duration::from_mins(1)),
            profile: None,
            processor_tuning: None,
            sequencer_tuning: None,
            vacuum_tuning: None,
            reconciler_tuning: None,
            tables: OutboxTables::default(),
            queue_declarations: Vec::new(),
        }
    }

    /// Select a custom table prefix for this outbox instance.
    ///
    /// The same prefix must be used when registering outbox migrations with
    /// [`super::outbox_migrations_with_prefix`].
    ///
    /// # Errors
    ///
    /// Returns [`OutboxError::InvalidTablePrefix`] if the prefix cannot be used
    /// as a portable SQL identifier.
    pub fn table_prefix(mut self, prefix: impl Into<String>) -> Result<Self, OutboxError> {
        self.tables = OutboxTables::new(prefix)?;
        Ok(self)
    }

    /// Maximum number of concurrent partition processors across all queues.
    ///
    /// Controls the global processor semaphore. Each partition processor
    /// acquires one permit before executing. Default: 4.
    ///
    /// # Panics
    ///
    /// Panics if `n` is 0.
    ///
    /// # Connection budget
    ///
    /// `max_connections >= processors + guaranteed + shared + producer_headroom`
    #[must_use]
    pub fn processors(mut self, n: usize) -> Self {
        assert!(n > 0, "processors must be > 0");
        self.processors = Some(n);
        self
    }

    /// Two-tier maintenance connection budget.
    ///
    /// - `guaranteed`: permits reserved exclusively for sequencer workers.
    /// - `shared`: permits available to both sequencer and maintenance tasks
    ///   (vacuum). Sequencers steal shared permits when maintenance is idle.
    ///
    /// Total sequencer workers = `guaranteed + shared`.
    /// Vacuum workers = `shared`.
    ///
    /// # Panics
    ///
    /// Panics if either `guaranteed` or `shared` is 0.
    ///
    /// # Connection budget
    ///
    /// `max_connections >= processors + guaranteed + shared + producer_headroom`
    ///
    /// Default: `guaranteed = 2, shared = 1`.
    #[must_use]
    pub fn maintenance(mut self, guaranteed: usize, shared: usize) -> Self {
        assert!(guaranteed > 0, "maintenance guaranteed must be > 0");
        assert!(shared > 0, "maintenance shared must be > 0");
        self.maintenance_guaranteed = Some(guaranteed);
        self.maintenance_shared = Some(shared);
        self
    }

    /// Max partitions the sequencer processes per cycle. Default: 128.
    ///
    /// # Panics
    ///
    /// Panics if `n` is 0.
    #[must_use]
    pub fn partition_batch_limit(mut self, n: u32) -> Self {
        assert!(n > 0, "partition_batch_limit must be > 0");
        self.partition_batch_limit = n;
        self
    }

    /// Max inner drain iterations per partition before yielding. Default: 8.
    ///
    /// # Panics
    ///
    /// Panics if `n` is 0.
    #[must_use]
    pub fn max_inner_iterations(mut self, n: u32) -> Self {
        assert!(n > 0, "max_inner_iterations must be > 0");
        self.max_inner_iterations = n;
        self
    }

    /// Periodic stats reporting interval. Default: 60s.
    /// `Duration::ZERO` disables stats collection entirely.
    #[must_use]
    pub fn stats_interval(mut self, d: Duration) -> Self {
        self.stats_interval = if d.is_zero() { None } else { Some(d) };
        self
    }

    /// Set a baseline tuning profile for all worker types.
    ///
    /// Individual worker tunings (e.g. [`processor_tuning()`](Self::processor_tuning))
    /// override the corresponding profile entry.
    ///
    /// Resolution order: per-worker tuning > profile > hardcoded default.
    #[must_use]
    pub fn profile(mut self, profile: OutboxProfile) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Override processor worker tuning. Takes precedence over the profile.
    #[must_use]
    pub fn processor_tuning(mut self, tuning: WorkerTuning) -> Self {
        self.processor_tuning = Some(tuning);
        self
    }

    /// Override sequencer worker tuning. Takes precedence over the profile.
    #[must_use]
    pub fn sequencer_tuning(mut self, tuning: WorkerTuning) -> Self {
        self.sequencer_tuning = Some(tuning);
        self
    }

    /// Override vacuum worker tuning. Takes precedence over the profile.
    #[must_use]
    pub fn vacuum_tuning(mut self, tuning: WorkerTuning) -> Self {
        self.vacuum_tuning = Some(tuning);
        self
    }

    /// Override reconciler worker tuning. Takes precedence over the profile.
    #[must_use]
    pub fn reconciler_tuning(mut self, tuning: WorkerTuning) -> Self {
        self.reconciler_tuning = Some(tuning);
        self
    }

    /// Begin building a queue registration.
    pub fn queue(self, name: &str, partitions: Partitions) -> QueueBuilder {
        QueueBuilder::new(self, name.to_owned(), partitions)
    }

    // ------------------------------------------------------------------
    // Private helpers for start()
    // ------------------------------------------------------------------

    /// Resolve per-worker tuning: per-worker > profile > hardcoded default.
    fn resolve_tuning(&self) -> ResolvedTuning {
        let default_profile = OutboxProfile::default();
        let profile = self.profile.as_ref().unwrap_or(&default_profile);
        let resolved = ResolvedTuning {
            processor: self
                .processor_tuning
                .clone()
                .unwrap_or_else(|| profile.processor.clone()),
            sequencer: self
                .sequencer_tuning
                .clone()
                .unwrap_or_else(|| profile.sequencer.clone()),
            vacuum: self
                .vacuum_tuning
                .clone()
                .unwrap_or_else(|| profile.vacuum.clone()),
            reconciler: self
                .reconciler_tuning
                .clone()
                .unwrap_or_else(|| profile.reconciler.clone()),
            notifier: profile.notifier.clone(),
            trace_sweeper: profile.trace_sweeper.clone(),
        };
        resolved.processor.validate();
        resolved.sequencer.validate();
        resolved.vacuum.validate();
        resolved.reconciler.validate();
        resolved.notifier.validate();
        resolved.trace_sweeper.validate();
        resolved
    }

    /// Spawn parallel sequencer workers (`guaranteed + shared`).
    fn spawn_sequencers(
        ctx: &mut StartContext<'_>,
        outbox: &Arc<Outbox>,
        prioritizer: &Arc<SharedPrioritizer>,
        tuning: &WorkerTuning,
        guaranteed_sem: &Arc<Semaphore>,
        shared_sem: &Arc<Semaphore>,
        count: usize,
    ) {
        for i in 0..count {
            #[allow(unused_mut)]
            let mut sequencer = Sequencer::new(
                outbox.config().sequencer.clone(),
                Arc::clone(outbox),
                ctx.db.clone(),
                Arc::clone(prioritizer),
            );
            let name = format!("sequencer-{i}");
            let mut builder = WorkerBuilder::<SequencerReport>::new(&name, ctx.cancel.clone())
                .pacing(tuning)
                .notifier(prioritizer.notifier())
                .notifier(Arc::clone(ctx.start_notify))
                .bulkhead(Bulkhead::new(
                    &name,
                    BulkheadConfig {
                        semaphore: ConcurrencyLimit::Tiered {
                            guaranteed: Arc::clone(guaranteed_sem),
                            shared: Arc::clone(shared_sem),
                        },
                        backoff: BackoffConfig::default(),
                    },
                ))
                .listener(TracingListener)
                .on_panic(PanicPolicy::CatchAndRetry);

            builder = register_stats(
                builder,
                ctx.stats_registry.as_ref(),
                "sequencer",
                Box::new(|any| {
                    any.downcast_ref::<SequencerReport>()
                        .map_or(0, |r| u64::from(r.rows_claimed))
                }),
            );

            let worker = builder.build(sequencer);
            ctx.task_set.spawn(&name, worker.run());
        }
    }

    /// Spawn parallel vacuum workers (one per shared permit).
    ///
    /// Vacuum workers use Fixed(shared) — not Tiered — so they can only run
    /// when no sequencer holds the shared permit. This is intentional: garbage
    /// collection is deferrable, sequencing is not. Under sustained load,
    /// sequencers preempt vacuum via the biased select in `Tiered::acquire()`.
    /// See the `maintenance()` doc comment for the full two-tier model.
    fn spawn_vacuum_workers(
        ctx: &mut StartContext<'_>,
        outbox: &Arc<Outbox>,
        collectable_traces: &Arc<Notify>,
        tuning: &WorkerTuning,
        shared_sem: &Arc<Semaphore>,
        count: usize,
    ) {
        for i in 0..count {
            #[allow(unused_mut)]
            let vacuum = VacuumTask::new(
                ctx.db.clone(),
                outbox.statements_arc(),
                tuning.batch_size as usize,
                Arc::clone(collectable_traces),
            );
            let name = format!("vacuum-{i}");
            let (poker_notify, _poker_handle) = poker(tuning.idle_interval, ctx.cancel.clone());
            let mut builder = WorkerBuilder::<VacuumReport>::new(&name, ctx.cancel.clone())
                .pacing(tuning)
                .notifier(poker_notify)
                .notifier(Arc::clone(ctx.start_notify))
                .bulkhead(Bulkhead::new(
                    &name,
                    BulkheadConfig {
                        semaphore: ConcurrencyLimit::Fixed(Arc::clone(shared_sem)),
                        backoff: BackoffConfig {
                            initial: Duration::from_millis(500),
                            max: Duration::from_mins(1),
                            ..Default::default()
                        },
                    },
                ))
                .listener(TracingListener)
                .on_panic(PanicPolicy::CatchAndRetry);

            builder = register_stats(
                builder,
                ctx.stats_registry.as_ref(),
                "vacuum",
                Box::new(|any| {
                    any.downcast_ref::<VacuumReport>()
                        .map_or(0, |r| r.rows_deleted)
                }),
            );

            let worker = builder.build(vacuum);
            ctx.task_set.spawn(&name, worker.run());
        }
    }

    /// Spawn the trace sweeper, on its own clock and nudged by the vacuum.
    fn spawn_trace_sweeper(
        ctx: &mut StartContext<'_>,
        outbox: &Arc<Outbox>,
        collectable_traces: &Arc<Notify>,
        tuning: &WorkerTuning,
    ) {
        let sweeper = super::workers::trace_sweeper::TraceSweeper {
            db: ctx.db.clone(),
            statements: outbox.statements_arc(),
            batch_size: tuning.batch_size as usize,
            periods: super::workers::trace_sweeper::TraceSweep::default(),
        };
        let name = "trace-sweeper";
        let (poker_notify, _poker_handle) = poker(tuning.idle_interval, ctx.cancel.clone());
        let worker = WorkerBuilder::new(name, ctx.cancel.clone())
            .pacing(tuning)
            .notifier(Arc::clone(collectable_traces))
            .notifier(poker_notify)
            .notifier(Arc::clone(ctx.start_notify))
            .listener(TracingListener)
            .on_panic(PanicPolicy::CatchAndRetry)
            .build(sweeper);
        ctx.task_set.spawn(name, worker.run());
    }

    /// Spawn the notifier: collects completions this instance owns but did not
    /// finish itself.
    ///
    /// Always spawned, and self-gating: it issues no query while nothing is
    /// outstanding, so a deployment that never subscribes pays a lock check
    /// per tick and nothing else.
    fn spawn_notifier(ctx: &mut StartContext<'_>, outbox: &Arc<Outbox>, tuning: &WorkerTuning) {
        let notifier = super::workers::notifier::Notifier {
            outbox: Arc::clone(outbox),
            db: ctx.db.clone(),
            batch_size: tuning.batch_size,
            next_look: Duration::from_millis(100),
        };
        let name = "notifier";
        let worker = WorkerBuilder::new(name, ctx.cancel.clone())
            .pacing(tuning)
            // No timer. It schedules itself while somebody is waiting, and
            // sleeps on this wakeup when nobody is - a subscription being
            // taken is the only event that gives it a reason to look.
            .notifier(outbox.mailbox().subscriptions().arrivals())
            .notifier(Arc::clone(ctx.start_notify))
            .listener(TracingListener)
            .on_panic(PanicPolicy::CatchAndRetry)
            .build(notifier);
        ctx.task_set.spawn(name, worker.run());
    }

    /// Spawn the stall reporter: tells subscribers which of their batches are
    /// stuck.
    ///
    /// Always spawned and self-gating, like the notifier, but on a stricter
    /// gate: it runs only while some caller holds a progress receiver, so a
    /// deployment that never watches for stalls issues no query here ever.
    fn spawn_stall_reporter(
        ctx: &mut StartContext<'_>,
        outbox: &Arc<Outbox>,
        tuning: &WorkerTuning,
    ) {
        let reporter = super::workers::stall_reporter::StallReporter {
            outbox: Arc::clone(outbox),
            db: ctx.db.clone(),
            batch_size: tuning.batch_size,
        };
        let name = "stall-reporter";
        let (poker_notify, _poker_handle) = poker(tuning.idle_interval, ctx.cancel.clone());
        let worker = WorkerBuilder::new(name, ctx.cancel.clone())
            .pacing(tuning)
            .notifier(poker_notify)
            .notifier(Arc::clone(ctx.start_notify))
            .listener(TracingListener)
            .on_panic(PanicPolicy::CatchAndRetry)
            .build(reporter);
        ctx.task_set.spawn(name, worker.run());
    }

    /// Spawn cold reconciler as a `WorkerAction` (ungated, poker-driven).
    fn spawn_cold_reconciler(
        ctx: &mut StartContext<'_>,
        outbox: &Arc<Outbox>,
        prioritizer: &Arc<SharedPrioritizer>,
        tuning: &WorkerTuning,
    ) {
        let reconciler = super::workers::reconciler::ColdReconciler {
            outbox: Arc::clone(outbox),
            db: ctx.db.clone(),
            prioritizer: Arc::clone(prioritizer),
        };
        let name = "cold-reconciler";
        let (poker_notify, _poker_handle) = poker(tuning.idle_interval, ctx.cancel.clone());
        let worker = WorkerBuilder::new(name, ctx.cancel.clone())
            .pacing(tuning)
            .notifier(poker_notify)
            .notifier(Arc::clone(ctx.start_notify))
            .listener(TracingListener)
            .on_panic(PanicPolicy::CatchAndRetry)
            .build(reconciler);
        ctx.task_set.spawn(name, worker.run());
    }

    /// Spawn stats reporter (if enabled).
    fn spawn_stats_reporter(
        ctx: &mut StartContext<'_>,
        stats_registry_shared: Option<Arc<std::sync::Mutex<StatsRegistry>>>,
        interval: Duration,
    ) {
        // Extract the registry — all workers have registered by now.
        #[allow(clippy::expect_used)]
        let registry = stats_registry_shared
            .expect("stats_registry_shared is Some when stats_interval is Some")
            .lock()
            .ok()
            .map(|mut guard| std::mem::replace(&mut *guard, StatsRegistry::new()))
            .expect("stats registry mutex not poisoned");
        let reporter = StatsReporter::new(Arc::new(registry));
        let name = "stats-reporter";
        let (poker_notify, _poker_handle) = poker(interval, ctx.cancel.clone());
        let worker = WorkerBuilder::new(name, ctx.cancel.clone())
            .notifier(poker_notify)
            .on_panic(PanicPolicy::CatchAndRetry)
            .build(reporter);
        ctx.task_set.spawn(name, worker.run());
    }

    /// Spawn background tasks and return a handle to the running pipeline.
    ///
    /// Registers all queues in the database, creates the sequencer and
    /// per-partition processors, then starts them as background tasks.
    ///
    /// # Errors
    ///
    /// Returns an error if queue registration fails (DB operation).
    ///
    /// # Panics
    ///
    /// Panics if the internal stats registry mutex is poisoned.
    pub async fn start(mut self) -> Result<OutboxHandle, OutboxError> {
        // 1. Resolve tuning
        let tuning = self.resolve_tuning();

        // 2. Build shared prioritizer — Outbox and sequencer workers both
        //    subscribe to its internal Notify for wakeups.
        let shared_prioritizer = Arc::new(SharedPrioritizer::new());

        let config = OutboxConfig {
            tables: self.tables.clone(),
            instance_id: super::types::InstanceId::default(),
            sequencer: SequencerConfig {
                batch_size: tuning.sequencer.batch_size,
                poll_interval: tuning.sequencer.idle_interval,
                partition_batch_limit: self.partition_batch_limit,
                max_inner_iterations: self.max_inner_iterations,
            },
        };
        let backend = self.db.sea_internal().get_database_backend();
        let outbox = Arc::new(Outbox::new_with_backend(config, backend));
        let cancel = CancellationToken::new();
        let mut task_set = TaskSet::new(cancel.clone());
        let start_notify = Arc::new(Notify::new());
        let partition_notify: DashMap<i64, Arc<Notify>> = DashMap::new();

        // Global processor semaphore — caps total concurrent partition processors
        let processor_sem = Arc::new(Semaphore::new(
            self.processors.unwrap_or(4).min(Semaphore::MAX_PERMITS),
        ));

        // Shared stats registry (wrapped in Mutex for processor factory access)
        let stats_registry_shared = if self.stats_interval.is_some() {
            Some(Arc::new(std::sync::Mutex::new(StatsRegistry::new())))
        } else {
            None
        };

        // 3. Register queues and spawn processor workers via factories
        for decl in &mut self.queue_declarations {
            outbox
                .register_queue(&self.db, &decl.name, decl.partitions.count())
                .await?;

            let partition_ids = outbox.partition_ids_for_queue(&decl.name);

            for &pid in &partition_ids {
                let notify = Arc::new(Notify::new());
                partition_notify.insert(pid, Arc::clone(&notify));
                let spawn_ctx = super::builder::SpawnContext {
                    pid,
                    db: self.db.clone(),
                    cancel: cancel.clone(),
                    partition_notify: notify,
                    processor_sem: Arc::clone(&processor_sem),
                    start_notify: Arc::clone(&start_notify),
                    outbox: Arc::clone(&outbox),
                    stats_registry: stats_registry_shared.clone(),
                    tuning: tuning.processor.clone(),
                };
                let (name, future) = decl.factory.spawn(spawn_ctx);
                task_set.spawn(name, future);
            }
        }

        // 4. Two-tier maintenance semaphores
        let guaranteed = self
            .maintenance_guaranteed
            .unwrap_or(2)
            .min(Semaphore::MAX_PERMITS);
        let shared = self
            .maintenance_shared
            .unwrap_or(1)
            .min(Semaphore::MAX_PERMITS);
        let guaranteed_sem = Arc::new(Semaphore::new(guaranteed));
        let shared_sem = Arc::new(Semaphore::new(shared));

        // 5. Wire partition notifiers for the sequencer
        let mut notify_map: HashMap<i64, Arc<Notify>> = HashMap::new();
        for entry in &partition_notify {
            notify_map.insert(*entry.key(), Arc::clone(entry.value()));
        }
        let notify_map = Arc::new(notify_map);
        outbox.set_partition_notify(notify_map).await;

        outbox
            .set_prioritizer(Arc::clone(&shared_prioritizer))
            .await;

        // 6. Eager reconciliation at startup
        super::workers::reconciler::reconcile_dirty(&outbox, &self.db, &shared_prioritizer).await;

        let mut ctx = StartContext {
            db: &self.db,
            cancel: &cancel,
            task_set: &mut task_set,
            start_notify: &start_notify,
            stats_registry: &stats_registry_shared,
        };

        // 7. Spawn sequencers
        let sequencer_count = guaranteed.saturating_add(shared);
        Self::spawn_sequencers(
            &mut ctx,
            &outbox,
            &shared_prioritizer,
            &tuning.sequencer,
            &guaranteed_sem,
            &shared_sem,
            sequencer_count,
        );

        // 8. Spawn cold reconciler
        Self::spawn_cold_reconciler(&mut ctx, &outbox, &shared_prioritizer, &tuning.reconciler);

        // 8b. Collect completion mail this instance owns
        Self::spawn_notifier(&mut ctx, &outbox, &tuning.notifier);

        // 8c. Report stalls to callers watching their batches. Paced with the
        // notifier on purpose: both are subscription-gated polls of the trace
        // table, and a caller learning its batch is stuck is worth the same
        // latency as learning it finished.
        Self::spawn_stall_reporter(&mut ctx, &outbox, &tuning.notifier);

        // 9. Spawn vacuum workers, and the sweeper they tell about their work
        let collectable_traces = Arc::new(Notify::new());
        Self::spawn_vacuum_workers(
            &mut ctx,
            &outbox,
            &collectable_traces,
            &tuning.vacuum,
            &shared_sem,
            shared,
        );
        Self::spawn_trace_sweeper(
            &mut ctx,
            &outbox,
            &collectable_traces,
            &tuning.trace_sweeper,
        );

        // 10. Spawn stats reporter (if enabled)
        if let Some(interval) = self.stats_interval {
            Self::spawn_stats_reporter(&mut ctx, stats_registry_shared.clone(), interval);
        }

        // 11. Signal all workers to start
        start_notify.notify_waiters();

        Ok(OutboxHandle {
            outbox,
            tasks: task_set,
        })
    }
}

/// A running outbox pipeline. Obtained by calling [`OutboxBuilder::start()`].
///
/// Provides access to the [`Outbox`] for enqueue operations and a
/// [`stop()`](Self::stop) method for graceful shutdown.
///
/// Drop safety: if `stop()` is never called, `TaskSet::Drop` cancels the
/// cancellation token, signaling all workers to exit.
pub struct OutboxHandle {
    outbox: Arc<Outbox>,
    tasks: TaskSet,
}

impl OutboxHandle {
    /// Returns the outbox for enqueue operations.
    #[must_use]
    pub fn outbox(&self) -> &Arc<Outbox> {
        &self.outbox
    }

    /// Cancel background tasks and join all handles. Consumes self.
    pub async fn stop(self) {
        self.tasks.shutdown().await;
        // Resolve anyone still awaiting a completion. Dropping the senders is
        // what makes an awaiting subscription yield `None`, which is the
        // documented signal that this process can no longer answer. Without
        // it the caller waits for ever: it necessarily holds the `Arc` that
        // owns the registry, so nothing else would ever drop them.
        self.outbox.mailbox().subscriptions().close();
    }
}
