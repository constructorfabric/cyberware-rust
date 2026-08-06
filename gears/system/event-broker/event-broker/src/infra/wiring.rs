//! Builds the real `IngestService`/`DeliveryService` pair over a shared
//! `Storage` - the single production wiring path (`module.rs::
//! register_rest()`), also used by `test_support::harness` so tests exercise
//! the same construction, not a second independently-maintained one
//! (`InMemoryDomainRepo`, which this supersedes - eb-single-process-
//! implementation design.md D2 risk mitigation).

use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::PolicyEnforcer;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::api::rest::state::HandlerState;
use crate::config::{LoaderConfig, StreamingConfig};
use crate::domain::backend::BackendResolver;
use crate::domain::consumer_group_coordinator::ConsumerGroupCoordinator;
use crate::domain::delivery::{DeliveryService, DeliveryServiceImpl};
use crate::domain::ingest::{IngestService, IngestServiceImpl};
use crate::domain::specification::SpecificationManager;
use crate::domain::streaming::lease::InProcessStreamLeases;
use crate::infra::loader::backend_source::BackendEventSource;
use crate::infra::loader::poll::PollPolicy;
use crate::infra::loader::scheduler::{DemandScheduler, SchedulerPolicy};
use crate::infra::loader::shard::ShardLoader;
use crate::infra::loader::topics::{TopicManager, TopicPolicy};
use crate::infra::partition_cache::demand::StarvationWeight;
use crate::infra::partition_cache::reclaim::{
    GapThresholdEvents, ReclaimPolicy, ResidencyLimitBytes,
};
use crate::infra::storage::Storage;

/// Takes the whole `StreamingConfig` rather than a heartbeat override.
///
/// The override existed because only the heartbeat was configurable and the
/// rest were literals at the call site. Every field is read now - batch bounds
/// and progress cadence reach the session, the heartbeat reaches its schedule -
/// so passing the struct is both smaller and the thing that makes "every knob
/// has a reader" checkable.
#[must_use]
pub fn build_handler_state(
    storage: Arc<Storage>,
    policy_enforcer: PolicyEnforcer,
    spec_manager: Arc<dyn SpecificationManager>,
    backend_resolver: Arc<dyn BackendResolver>,
    topics: Arc<TopicManager>,
    leases: Arc<InProcessStreamLeases>,
    streaming: StreamingConfig,
) -> HandlerState {
    let ingest: Arc<dyn IngestService> = Arc::new(IngestServiceImpl::new(
        Arc::clone(&storage),
        policy_enforcer.clone(),
        Arc::clone(&spec_manager),
        Arc::clone(&backend_resolver),
    ));
    let groups = Arc::new(ConsumerGroupCoordinator::new());
    let delivery_impl = DeliveryServiceImpl::new(
        storage,
        policy_enforcer,
        spec_manager,
        backend_resolver,
        groups,
        topics,
        leases,
        streaming,
    );
    let delivery: Arc<dyn DeliveryService> = Arc::new(delivery_impl);
    HandlerState { ingest, delivery }
}

/// The partition caches this instance serves from, built from configuration.
///
/// Created in `init` rather than here-and-there because two callers need the
/// same one: `register_rest` hands it to the delivery service so a session can
/// attach readers, and `serve` hands it to the loader so those readers get
/// filled. Two managers would mean two sets of caches for one partition, with
/// readers on each believing they had its state.
#[must_use]
pub fn build_topic_manager(cfg: &LoaderConfig) -> Arc<TopicManager> {
    let reclaim = ReclaimPolicy::new(
        GapThresholdEvents(cfg.gap_threshold_events),
        ResidencyLimitBytes(cfg.residency_limit_bytes),
    );
    let policy = TopicPolicy::builder(reclaim)
        .fetch_max_events(cfg.fetch_max_events)
        .poll(
            PollPolicy::from_floor(Duration::from_millis(cfg.poll_floor_ms))
                .up_to(Duration::from_millis(cfg.poll_ceiling_ms)),
        )
        .build();
    Arc::new(TopicManager::new(policy))
}

/// Spawns the instance's loader: what fills the caches ahead of readers, and
/// what reclaims behind them.
///
/// Takes the `CancellationToken` `serve` already owns, so shutdown cancels and
/// joins rather than dropping a fetch in flight against a closing pool. Must be
/// called from inside a runtime - which is why it belongs to `serve` and not to
/// the synchronous `register_rest`.
#[must_use]
pub fn spawn_loader(
    cfg: &LoaderConfig,
    topics: Arc<TopicManager>,
    spec_manager: Arc<dyn SpecificationManager>,
    backend_resolver: Arc<dyn BackendResolver>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let source = Arc::new(BackendEventSource::new(spec_manager, backend_resolver));
    let scheduler = Arc::new(DemandScheduler::new(
        source,
        Arc::clone(&topics),
        SchedulerPolicy::with_pool(cfg.pool_permits)
            .starvation_weight(StarvationWeight(cfg.starvation_weight))
            .build(),
    ));
    ShardLoader::new(scheduler, topics, Duration::from_millis(cfg.tick_ms)).spawn(cancel)
}
