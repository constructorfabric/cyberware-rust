use std::time::Duration;

use thiserror::Error;

use super::tables::OutboxTables;

/// Default batch size for the sequencer (rows per cycle).
pub const DEFAULT_SEQUENCER_BATCH_SIZE: u32 = 1000;

/// Default poll interval (safety net fallback for sequencer and processors).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_mins(10);

/// Default partition batch limit for the sequencer (max partitions per cycle).
pub const DEFAULT_PARTITION_BATCH_LIMIT: u32 = 128;

/// Default max inner iterations per partition before yielding.
pub const DEFAULT_MAX_INNER_ITERATIONS: u32 = 8;

/// Number of partitions for a queue. Must be a power of 2 in `1..=64`.
///
/// ```
/// use toolkit_db::outbox::Partitions;
/// let p = Partitions::of(4);
/// assert_eq!(p.count(), 4);
/// ```
///
/// Invalid values panic at compile time when used as a const:
/// ```compile_fail
/// use toolkit_db::outbox::Partitions;
/// const BAD: Partitions = Partitions::of(3); // not a power of 2
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Partitions(u16);

impl Partitions {
    /// Create a partition count.
    ///
    /// # Panics
    ///
    /// Panics if `n` is not a power of 2 in `1..=64`.
    #[must_use]
    pub const fn of(n: u16) -> Self {
        assert!(
            n >= 1 && n <= 64 && n.is_power_of_two(),
            "partition count must be a power of 2 between 1 and 64"
        );
        Self(n)
    }

    /// Returns the numeric partition count.
    #[must_use]
    pub const fn count(self) -> u16 {
        self.0
    }
}

/// Identity of one running outbox instance, generated rather than configured.
///
/// A trace completion is delivered to the instance that enqueued the batch,
/// whichever instance processed the entities, and this is how that instance is
/// addressed. It is generated per process and never surfaced as a setting,
/// because it only has to be unique among *running* processes: a restarted
/// process holds no subscription for anything to be delivered to, so a stable
/// name would buy nothing. Mail addressed to a process that is gone is
/// collected by the sweep, and the durable answer for a restarted process is
/// [`Outbox::trace_status`](super::Outbox::trace_status).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstanceId(String);

impl InstanceId {
    /// Name an instance explicitly. Tests only; a running outbox generates its
    /// own.
    #[cfg(test)]
    pub(crate) fn new(instance_id: impl Into<String>) -> Self {
        Self(instance_id.into())
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for InstanceId {
    /// A fresh per-process name, unique to this process and lost on restart.
    fn default() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier for an enqueued outbox message (the `toolkit_outbox_incoming.id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutboxMessageId(pub i64);

/// Errors from the outbox subsystem.
#[derive(Debug, Error)]
pub enum OutboxError {
    #[error("queue '{0}' is not registered")]
    QueueNotRegistered(String),

    #[error("partition {partition} is out of range for queue '{queue}' (max {max})")]
    PartitionOutOfRange {
        queue: String,
        partition: u32,
        max: u32,
    },

    #[error("payload size {size} exceeds maximum {max}")]
    PayloadTooLarge { size: usize, max: usize },

    #[error("partition count mismatch for queue '{queue}': expected {expected}, found {found}")]
    PartitionCountMismatch {
        queue: String,
        expected: u16,
        found: usize,
    },

    #[error("a traced batch must carry at least one entity")]
    EmptyTracedBatch,

    #[error("invalid queue name: {reason}")]
    InvalidQueueName { reason: &'static str },

    #[error("invalid payload type: {reason}")]
    InvalidPayloadType { reason: &'static str },

    #[error("invalid trace: {reason}")]
    InvalidTrace { reason: &'static str },

    #[error("trace size {size} exceeds maximum {max}")]
    TraceTooLong { size: usize, max: usize },

    #[error("invalid outbox table prefix: {reason}")]
    InvalidTablePrefix { reason: &'static str },

    #[error(transparent)]
    Database(#[from] sea_orm::DbErr),
}

/// Configuration for the outbox subsystem.
#[derive(Debug, Clone, Default)]
pub struct OutboxConfig {
    pub(crate) tables: OutboxTables,
    pub(crate) instance_id: InstanceId,
    pub sequencer: SequencerConfig,
}

/// Configuration for the sequencer background task.
#[derive(Debug, Clone)]
pub struct SequencerConfig {
    pub batch_size: u32,
    pub poll_interval: Duration,
    /// Max partitions to process per sequencer cycle. Default: 128.
    pub partition_batch_limit: u32,
    /// Max inner drain iterations per partition before yielding. Default: 8.
    pub max_inner_iterations: u32,
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_SEQUENCER_BATCH_SIZE,
            poll_interval: DEFAULT_POLL_INTERVAL,
            partition_batch_limit: DEFAULT_PARTITION_BATCH_LIMIT,
            max_inner_iterations: DEFAULT_MAX_INNER_ITERATIONS,
        }
    }
}

/// Lease configuration for leased handlers.
///
/// Controls how long the processor holds a partition lease and how much
/// headroom is reserved for the ack transaction after the handler finishes.
#[derive(Debug, Clone, Copy)]
pub struct LeaseConfig {
    /// Total lease duration held on the partition. Default: 30s.
    pub duration: Duration,
    /// Time reserved after handler cancellation for the ack DB round-trip.
    /// The handler cancel point fires at `duration - headroom`. Default: 2s.
    pub headroom: Duration,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(30),
            headroom: Duration::from_secs(2),
        }
    }
}

impl LeaseConfig {
    /// Validate that headroom is strictly less than duration.
    /// Panics at build time if invalid.
    pub(crate) fn validate(&self) {
        assert!(
            self.headroom < self.duration,
            "LeaseConfig: headroom ({:?}) must be less than duration ({:?})",
            self.headroom,
            self.duration,
        );
    }

    /// Duration from lease start until handler cancellation.
    pub(crate) fn handler_budget(&self) -> Duration {
        // Safety: validate() ensures headroom < duration at build time.
        self.duration.saturating_sub(self.headroom)
    }
}

// ---------------------------------------------------------------------------
// WorkerTuning — unified per-worker configuration with adaptive pacing
// ---------------------------------------------------------------------------

/// Per-worker timing and behavior configuration.
///
/// Controls adaptive pacing (min/active/idle intervals with ramp-down),
/// retry backoff for handler Retry/Reject, and batch degradation threshold.
/// All timing decisions live here — workers just report what happened
/// (Proceed/Idle/Sleep), the loop decides how long to wait.
///
/// Use named profiles for common patterns, or the builder for fine-tuning:
///
/// ```ignore
/// // Profile directly
/// let tuning = WorkerTuning::processor_low_latency();
///
/// // Profile + tweaks
/// let tuning = WorkerTuning::processor_high_throughput()
///     .batch_size(50)
///     .retry_base(Duration::from_secs(5));
///
/// // On the outbox builder
/// Outbox::builder(db)
///     .profile(OutboxProfile::high_throughput())
///     .processor_tuning(WorkerTuning::processor_high_throughput().batch_size(50))
///     .start().await?;
/// ```
#[derive(Debug, Clone)]
pub struct WorkerTuning {
    /// Max items per execution cycle.
    pub batch_size: u32,
    /// Fastest pace — floor for sustained high-throughput work.
    /// The worker ramps down to this after consecutive Proceeds.
    pub min_interval: Duration,
    /// Starting pace after waking from Idle. Ramps down toward
    /// `min_interval` on consecutive Proceeds.
    pub active_interval: Duration,
    /// Safety-net poll interval while Idle (poker timer).
    /// If no notifier fires within this window, the worker wakes.
    pub idle_interval: Duration,
    /// Subtracted per consecutive Proceed (ramp-down step).
    pub ramp_step: Duration,
    /// Base delay on handler Retry/Reject. Escalates exponentially
    /// via `PartitionMode::current_backoff()`. Processor-only.
    pub retry_base: Duration,
    /// Max retry delay (exponential cap). Processor-only.
    pub retry_max: Duration,
    /// Consecutive handler failures before batch size degrades.
    /// Processor-only. Set to 1 for immediate degradation.
    pub degradation_threshold: u32,
    /// Lease duration for leased mode partition locks.
    /// Processor-only. Ignored for transactional mode. Default: 30s.
    pub lease_duration: Duration,
}

impl WorkerTuning {
    // -- Consume-return-Self builder methods --

    #[must_use]
    pub fn batch_size(mut self, n: u32) -> Self {
        self.batch_size = n;
        self
    }

    #[must_use]
    pub fn min_interval(mut self, d: Duration) -> Self {
        self.min_interval = d;
        self
    }

    #[must_use]
    pub fn active_interval(mut self, d: Duration) -> Self {
        self.active_interval = d;
        self
    }

    #[must_use]
    pub fn idle_interval(mut self, d: Duration) -> Self {
        self.idle_interval = d;
        self
    }

    #[must_use]
    pub fn ramp_step(mut self, d: Duration) -> Self {
        self.ramp_step = d;
        self
    }

    #[must_use]
    pub fn retry_base(mut self, d: Duration) -> Self {
        self.retry_base = d;
        self
    }

    #[must_use]
    pub fn retry_max(mut self, d: Duration) -> Self {
        self.retry_max = d;
        self
    }

    #[must_use]
    pub fn degradation_threshold(mut self, n: u32) -> Self {
        self.degradation_threshold = n;
        self
    }

    #[must_use]
    pub fn lease_duration(mut self, d: Duration) -> Self {
        self.lease_duration = d;
        self
    }

    // -- Per-worker-type constructors (defaults) --

    /// Processor defaults (balanced profile).
    #[must_use]
    pub fn processor() -> Self {
        Self::processor_default()
    }

    /// Sequencer defaults (balanced profile).
    #[must_use]
    pub fn sequencer() -> Self {
        Self::sequencer_default()
    }

    /// Vacuum defaults.
    #[must_use]
    pub fn vacuum() -> Self {
        Self {
            batch_size: 10_000,
            min_interval: Duration::from_secs(1),
            active_interval: Duration::from_secs(1),
            idle_interval: Duration::from_hours(1),
            ramp_step: Duration::ZERO,
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_mins(1),
            degradation_threshold: 1,
            lease_duration: Duration::from_secs(30),
        }
    }

    /// Notifier defaults.
    ///
    /// The intervals here are not what paces it: it returns `Sleep` with its
    /// own widening delay while a caller is waiting, and `Idle` when none is,
    /// so only `batch_size` and the panic policy matter. A fixed idle interval
    /// would be wrong in both directions at once - too slow for a batch that
    /// just finished, too eager for one that will take minutes.
    #[must_use]
    pub fn notifier() -> Self {
        Self {
            batch_size: 100,
            min_interval: Duration::from_millis(100),
            active_interval: Duration::from_millis(100),
            idle_interval: Duration::from_millis(250),
            ramp_step: Duration::ZERO,
            retry_base: Duration::from_millis(100),
            retry_max: Duration::from_secs(30),
            degradation_threshold: 1,
            // Unused by this worker; it holds no lease.
            lease_duration: Duration::from_secs(30),
        }
    }

    /// Trace-sweeper defaults.
    ///
    /// Collection is mostly time-driven - a delivered trace becomes
    /// collectable by nothing more than the clock - so this runs on its own
    /// slow pace rather than the body collector's, and is additionally nudged
    /// when the vacuum has deleted bodies.
    #[must_use]
    pub fn trace_sweeper() -> Self {
        Self {
            batch_size: 500,
            min_interval: Duration::from_secs(1),
            active_interval: Duration::from_secs(1),
            idle_interval: Duration::from_mins(5),
            ramp_step: Duration::ZERO,
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_mins(1),
            degradation_threshold: 1,
            lease_duration: Duration::from_secs(30),
        }
    }

    /// Reconciler defaults.
    #[must_use]
    pub fn reconciler() -> Self {
        Self {
            batch_size: 1,
            min_interval: Duration::from_secs(1),
            active_interval: Duration::from_secs(1),
            idle_interval: Duration::from_mins(1),
            ramp_step: Duration::ZERO,
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_mins(1),
            degradation_threshold: 1,
            lease_duration: Duration::from_secs(30),
        }
    }

    // -- Processor profiles --

    /// Default processor profile. Conservative — start gentle, opt into
    /// faster profiles when needed.
    #[must_use]
    pub fn processor_default() -> Self {
        Self {
            batch_size: 10,
            min_interval: Duration::from_millis(100),
            active_interval: Duration::from_millis(500),
            idle_interval: Duration::from_mins(10),
            ramp_step: Duration::from_millis(50),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_mins(1),
            degradation_threshold: 2,
            lease_duration: Duration::from_secs(30),
        }
    }

    /// Low-latency processor profile. Real-time notifications, chat.
    /// Fast pacing, aggressive retry. Batch size is moderate - latency
    /// comes from fast intervals, not small batches.
    #[must_use]
    pub fn processor_low_latency() -> Self {
        Self {
            batch_size: 10,
            min_interval: Duration::from_millis(1),
            active_interval: Duration::from_millis(2),
            idle_interval: Duration::from_mins(1),
            ramp_step: Duration::from_millis(1),
            retry_base: Duration::from_millis(100),
            retry_max: Duration::from_secs(10),
            degradation_threshold: 3,
            lease_duration: Duration::from_secs(30),
        }
    }

    /// High-throughput processor profile. Bulk ETL, analytics.
    /// Large batches, fast floor, throughput from batch size.
    #[must_use]
    pub fn processor_high_throughput() -> Self {
        Self {
            batch_size: 100,
            min_interval: Duration::from_millis(1),
            active_interval: Duration::from_millis(20),
            idle_interval: Duration::from_mins(10),
            ramp_step: Duration::from_millis(2),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_mins(1),
            degradation_threshold: 2,
            lease_duration: Duration::from_secs(30),
        }
    }

    /// Relaxed processor profile. Background jobs, email digests.
    /// Slow pace, large backoff, immediate degradation.
    #[must_use]
    pub fn processor_relaxed() -> Self {
        Self {
            batch_size: 10,
            min_interval: Duration::from_millis(100),
            active_interval: Duration::from_millis(500),
            idle_interval: Duration::from_mins(10),
            ramp_step: Duration::from_millis(50),
            retry_base: Duration::from_secs(5),
            retry_max: Duration::from_mins(5),
            degradation_threshold: 1,
            lease_duration: Duration::from_secs(30),
        }
    }

    // -- Sequencer profiles --

    /// Default sequencer profile. Conservative — matches processor default pacing.
    #[must_use]
    pub fn sequencer_default() -> Self {
        Self {
            batch_size: 1000,
            min_interval: Duration::from_millis(100),
            active_interval: Duration::from_millis(500),
            idle_interval: Duration::from_mins(10),
            ramp_step: Duration::from_millis(50),
            retry_base: Duration::from_millis(100),
            retry_max: Duration::from_secs(30),
            degradation_threshold: 1,
            lease_duration: Duration::from_secs(30),
        }
    }

    /// Low-latency sequencer profile.
    #[must_use]
    pub fn sequencer_low_latency() -> Self {
        Self {
            batch_size: 500,
            min_interval: Duration::ZERO,
            active_interval: Duration::from_millis(1),
            idle_interval: Duration::from_mins(1),
            ramp_step: Duration::ZERO,
            retry_base: Duration::from_millis(100),
            retry_max: Duration::from_secs(30),
            degradation_threshold: 1,
            lease_duration: Duration::from_secs(30),
        }
    }

    /// High-throughput sequencer profile.
    #[must_use]
    pub fn sequencer_high_throughput() -> Self {
        Self {
            batch_size: 2000,
            min_interval: Duration::from_millis(10),
            active_interval: Duration::from_millis(100),
            idle_interval: Duration::from_mins(10),
            ramp_step: Duration::from_millis(10),
            retry_base: Duration::from_millis(100),
            retry_max: Duration::from_secs(30),
            degradation_threshold: 1,
            lease_duration: Duration::from_secs(30),
        }
    }

    /// Relaxed sequencer profile.
    #[must_use]
    pub fn sequencer_relaxed() -> Self {
        Self {
            batch_size: 1000,
            min_interval: Duration::from_millis(100),
            active_interval: Duration::from_millis(500),
            idle_interval: Duration::from_mins(10),
            ramp_step: Duration::from_millis(100),
            retry_base: Duration::from_millis(100),
            retry_max: Duration::from_secs(30),
            degradation_threshold: 1,
            lease_duration: Duration::from_secs(30),
        }
    }
}

impl From<&WorkerTuning> for super::taskward::PacingConfig {
    fn from(t: &WorkerTuning) -> Self {
        Self {
            min_interval: t.min_interval,
            active_interval: t.active_interval,
            ramp_step: t.ramp_step,
        }
    }
}

impl From<WorkerTuning> for super::taskward::PacingConfig {
    fn from(t: WorkerTuning) -> Self {
        Self::from(&t)
    }
}

impl WorkerTuning {
    /// Validate that field invariants hold.
    ///
    /// # Panics
    ///
    /// Panics if `min_interval > active_interval`, `retry_base > retry_max`,
    /// `batch_size == 0`, `retry_base` is zero, or `degradation_threshold` is zero.
    pub fn validate(&self) {
        assert!(
            self.batch_size >= 1,
            "WorkerTuning: batch_size must be >= 1"
        );
        assert!(
            self.min_interval <= self.active_interval,
            "WorkerTuning: min_interval ({:?}) must be <= active_interval ({:?})",
            self.min_interval,
            self.active_interval
        );
        assert!(
            !self.retry_base.is_zero(),
            "WorkerTuning: retry_base must be > 0 (got ZERO)"
        );
        assert!(
            self.retry_base <= self.retry_max,
            "WorkerTuning: retry_base ({:?}) must be <= retry_max ({:?})",
            self.retry_base,
            self.retry_max
        );
        assert!(
            self.degradation_threshold >= 1,
            "WorkerTuning: degradation_threshold must be >= 1 (got {})",
            self.degradation_threshold
        );
    }
}

// ---------------------------------------------------------------------------
// OutboxProfile — global profile bundling all worker tunings
// ---------------------------------------------------------------------------

/// Global outbox profile that sets all worker types at once.
///
/// Use `.profile()` on `OutboxBuilder` for one-line configuration.
/// Per-worker overrides (`.processor_tuning()`, etc.) take precedence.
///
/// ```ignore
/// Outbox::builder(db)
///     .profile(OutboxProfile::high_throughput())
///     .processor_tuning(WorkerTuning::processor_high_throughput().batch_size(50))
///     .start().await?;
/// ```
#[derive(Debug, Clone)]
pub struct OutboxProfile {
    pub sequencer: WorkerTuning,
    pub processor: WorkerTuning,
    pub vacuum: WorkerTuning,
    pub reconciler: WorkerTuning,
    pub notifier: WorkerTuning,
    pub trace_sweeper: WorkerTuning,
}

impl OutboxProfile {
    /// Balanced profile. General purpose.
    #[must_use]
    pub fn default_profile() -> Self {
        Self {
            sequencer: WorkerTuning::sequencer_default(),
            processor: WorkerTuning::processor_default(),
            vacuum: WorkerTuning::vacuum(),
            reconciler: WorkerTuning::reconciler(),
            notifier: WorkerTuning::notifier(),
            trace_sweeper: WorkerTuning::trace_sweeper(),
        }
    }

    /// Low-latency profile. Real-time notifications, chat.
    #[must_use]
    pub fn low_latency() -> Self {
        Self {
            sequencer: WorkerTuning::sequencer_low_latency(),
            processor: WorkerTuning::processor_low_latency(),
            vacuum: WorkerTuning::vacuum(),
            reconciler: WorkerTuning::reconciler().idle_interval(Duration::from_secs(30)),
            // A caller waiting on a completion in a low-latency deployment is
            // waiting on a chat message, not a nightly job.
            notifier: WorkerTuning::notifier().idle_interval(Duration::from_millis(50)),
            trace_sweeper: WorkerTuning::trace_sweeper(),
        }
    }

    /// High-throughput profile. Bulk ETL, analytics.
    #[must_use]
    pub fn high_throughput() -> Self {
        Self {
            sequencer: WorkerTuning::sequencer_high_throughput(),
            processor: WorkerTuning::processor_high_throughput(),
            vacuum: WorkerTuning::vacuum(),
            reconciler: WorkerTuning::reconciler(),
            notifier: WorkerTuning::notifier().batch_size(500),
            trace_sweeper: WorkerTuning::trace_sweeper(),
        }
    }

    /// Relaxed profile. Background jobs, email digests.
    #[must_use]
    pub fn relaxed() -> Self {
        Self {
            sequencer: WorkerTuning::sequencer_relaxed(),
            processor: WorkerTuning::processor_relaxed(),
            vacuum: WorkerTuning::vacuum(),
            reconciler: WorkerTuning::reconciler().idle_interval(Duration::from_mins(2)),
            notifier: WorkerTuning::notifier().idle_interval(Duration::from_secs(1)),
            trace_sweeper: WorkerTuning::trace_sweeper(),
        }
    }
}

impl Default for OutboxProfile {
    fn default() -> Self {
        Self::default_profile()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    // -- Bug 3: validate() doesn't catch zero retry_base or zero degradation_threshold --

    #[test]
    #[should_panic(expected = "retry_base must be > 0")]
    fn validate_rejects_zero_retry_base() {
        WorkerTuning::processor_default()
            .retry_base(Duration::ZERO)
            .validate();
    }

    #[test]
    #[should_panic(expected = "degradation_threshold must be >= 1")]
    fn validate_rejects_zero_degradation_threshold() {
        WorkerTuning::processor_default()
            .degradation_threshold(0)
            .validate();
    }

    #[test]
    fn lease_config_default() {
        let cfg = LeaseConfig::default();
        assert_eq!(cfg.duration, Duration::from_secs(30));
        assert_eq!(cfg.headroom, Duration::from_secs(2));
        assert_eq!(cfg.handler_budget(), Duration::from_secs(28));
    }

    #[test]
    #[should_panic(expected = "headroom")]
    fn lease_config_rejects_headroom_equal_to_duration() {
        LeaseConfig {
            duration: Duration::from_secs(5),
            headroom: Duration::from_secs(5),
        }
        .validate();
    }

    #[test]
    #[should_panic(expected = "headroom")]
    fn lease_config_rejects_headroom_greater_than_duration() {
        LeaseConfig {
            duration: Duration::from_secs(5),
            headroom: Duration::from_secs(10),
        }
        .validate();
    }
}
