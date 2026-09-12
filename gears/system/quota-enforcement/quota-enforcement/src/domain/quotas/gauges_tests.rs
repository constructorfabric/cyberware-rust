#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use quota_enforcement_sdk::testing::{InMemoryStorage, quota_draft, test_metric, test_subject};
use quota_enforcement_sdk::{
    CapPatch, QuotaEnforcementStoragePluginV1, QuotaId, QuotaPatch, StorageError,
};
use tokio_util::sync::CancellationToken;
use toolkit_security::AccessScope;

use super::{GaugeTiming, LifecycleGaugeRefresher, RefreshError};
use crate::domain::ports::lifecycle_gauges::{LifecycleCounts, LifecycleGaugeSink};
use crate::domain::ports::metric_registry::MetricMode;
use crate::test_support::{FakeMetricRegistry, RecordingGaugeSink, ctx, gated_counter, tenant};

const REFRESH: Duration = Duration::from_secs(10);
const DEADLINE: Duration = Duration::from_secs(5);
const STALE_AFTER: Duration = Duration::from_secs(25);

fn timing() -> GaugeTiming {
    GaugeTiming {
        refresh: REFRESH,
        refresh_deadline: DEADLINE,
        stale_after: STALE_AFTER,
    }
}

struct Harness {
    storage: Arc<InMemoryStorage>,
    registry: Arc<FakeMetricRegistry>,
    sink: Arc<RecordingGaugeSink>,
    refresher: Arc<LifecycleGaugeRefresher>,
}

impl Harness {
    fn new() -> Self {
        let storage = Arc::new(InMemoryStorage::new());
        let registry = Arc::new(FakeMetricRegistry::empty());
        registry.add(test_metric().as_str(), gated_counter());
        let sink = Arc::new(RecordingGaugeSink::default());
        let refresher = LifecycleGaugeRefresher::new(
            storage.clone(),
            registry.clone(),
            sink.clone() as Arc<dyn LifecycleGaugeSink>,
            timing(),
        );
        Self {
            storage,
            registry,
            sink,
            refresher,
        }
    }

    async fn seed(&self, cap: Option<u64>) -> QuotaId {
        self.storage
            .create_quota(
                &ctx(),
                &AccessScope::for_tenant(tenant().as_uuid()),
                quota_draft(test_subject("u1"), cap),
                &[],
            )
            .await
            .expect("seed")
    }

    async fn refresh(&self) -> Result<LifecycleCounts, RefreshError> {
        self.refresher.refresh_once(&CancellationToken::new()).await
    }
}

fn counts(cap_zero: u64, cap_unbounded: u64, for_direct_metric: u64) -> LifecycleCounts {
    LifecycleCounts {
        cap_zero,
        cap_unbounded,
        for_direct_metric,
    }
}

#[tokio::test]
async fn a_fresh_refresher_reads_the_counts_storage_holds() {
    let h = Harness::new();
    h.seed(Some(0)).await;
    h.seed(None).await;
    h.seed(Some(10)).await;
    assert_eq!(h.refresh().await.expect("refresh"), counts(1, 1, 0));
    assert!(
        h.sink.publications().is_empty(),
        "refresh_once publishes nothing"
    );
}

#[tokio::test]
async fn deactivation_and_cap_changes_move_the_counts_at_the_next_refresh() {
    let h = Harness::new();
    let zero = h.seed(Some(0)).await;
    let unbounded = h.seed(None).await;
    assert_eq!(h.refresh().await.expect("refresh"), counts(1, 1, 0));
    h.storage
        .deactivate_quota(
            &ctx(),
            &AccessScope::for_tenant(tenant().as_uuid()),
            zero,
            &[],
        )
        .await
        .expect("deactivate");
    assert_eq!(h.refresh().await.expect("refresh"), counts(0, 1, 0));
    h.storage
        .update_quota(
            &ctx(),
            &AccessScope::for_tenant(tenant().as_uuid()),
            unbounded,
            QuotaPatch {
                cap: Some(CapPatch::Bounded(0)),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect("bind to zero");
    assert_eq!(h.refresh().await.expect("refresh"), counts(1, 0, 0));
}

#[tokio::test]
async fn the_direct_count_follows_the_current_classification_in_both_directions() {
    let h = Harness::new();
    h.seed(Some(10)).await;
    h.seed(Some(20)).await;
    assert_eq!(h.refresh().await.expect("gated").for_direct_metric, 0);
    h.registry
        .set_mode(test_metric().as_str(), MetricMode::Direct);
    assert_eq!(h.refresh().await.expect("direct").for_direct_metric, 2);
    h.registry
        .set_mode(test_metric().as_str(), MetricMode::QuotaGated);
    assert_eq!(h.refresh().await.expect("gated again").for_direct_metric, 0);
    h.registry.remove(test_metric().as_str());
    assert_eq!(
        h.refresh()
            .await
            .expect("removed metric is flagged, not counted")
            .for_direct_metric,
        0
    );
}

#[tokio::test]
async fn a_stale_classification_fails_the_refresh() {
    let h = Harness::new();
    h.seed(Some(10)).await;
    h.registry.set_stale(test_metric().as_str());
    let err = h.refresh().await.expect_err("stale");
    assert_eq!(
        err,
        RefreshError::StaleClassification {
            metric: test_metric().as_str().to_owned()
        }
    );
}

#[tokio::test(start_paused = true)]
async fn the_loop_publishes_keeps_the_sample_through_failures_withdraws_it_and_recovers() {
    let h = Harness::new();
    h.seed(Some(0)).await;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(h.refresher.clone().run(cancel.clone()));

    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(h.sink.publications(), vec![Some(counts(1, 0, 0))]);

    h.storage
        .fail_with(StorageError::Unavailable("db down".into()));
    tokio::time::sleep(REFRESH + Duration::from_millis(10)).await;
    assert_eq!(
        h.sink.publications().len(),
        1,
        "a failed refresh keeps the last sample"
    );
    // Expiry falls inside the sleep between two refreshes and fires at once.
    tokio::time::sleep(STALE_AFTER).await;
    assert_eq!(h.sink.last(), Some(None), "withdrawn after stale_after");

    h.storage.clear_failure();
    tokio::time::sleep(REFRESH + Duration::from_millis(10)).await;
    assert_eq!(
        h.sink.publications(),
        vec![Some(counts(1, 0, 0)), None, Some(counts(1, 0, 0))],
        "exactly one withdrawal between the outage and the recovery"
    );

    // The disarmed expiry never fires again while refreshes keep succeeding.
    tokio::time::sleep(STALE_AFTER * 2).await;
    assert!(
        h.sink.publications().iter().skip(2).all(Option::is_some),
        "{:?}",
        h.sink.publications()
    );

    cancel.cancel();
    task.await.expect("run returns");
    assert_eq!(
        h.sink.last(),
        Some(None),
        "cancellation withdraws the sample"
    );
}

#[tokio::test(start_paused = true)]
async fn a_hanging_dependency_trips_the_deadline_and_then_the_staleness_bound() {
    let h = Harness::new();
    h.seed(Some(0)).await;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(h.refresher.clone().run(cancel.clone()));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(h.sink.last(), Some(Some(counts(1, 0, 0))));

    h.registry.hang();
    // Second refresh starts at REFRESH and times out at REFRESH + DEADLINE;
    // no cooperative cancel is involved.
    tokio::time::sleep(REFRESH + DEADLINE + Duration::from_millis(10)).await;
    assert_eq!(
        h.sink.publications().len(),
        1,
        "the sample survives the timeout"
    );
    tokio::time::sleep(STALE_AFTER).await;
    assert_eq!(
        h.sink.last(),
        Some(None),
        "withdrawn while the dependency still hangs"
    );

    cancel.cancel();
    task.await.expect("run returns");
}

#[tokio::test(start_paused = true)]
async fn an_abort_mid_refresh_withdraws_the_sample_and_discards_the_result() {
    let h = Harness::new();
    h.seed(Some(0)).await;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(h.refresher.clone().run(cancel.clone()));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(h.sink.publications().len(), 1);

    h.registry.hang();
    tokio::time::sleep(REFRESH + Duration::from_millis(10)).await;
    task.abort();
    let outcome = task.await;
    assert!(outcome.is_err_and(|e| e.is_cancelled()), "aborted");
    assert_eq!(
        h.sink.last(),
        Some(None),
        "the drop guard withdrew the sample"
    );
    tokio::time::sleep(STALE_AFTER * 2).await;
    assert_eq!(
        h.sink.publications().len(),
        2,
        "nothing is published after the abort"
    );
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_refresh_never_publishes_and_leadership_handover_moves_the_sample() {
    let a = Harness::new();
    a.seed(Some(0)).await;
    let cancel_a = CancellationToken::new();
    let task_a = tokio::spawn(a.refresher.clone().run(cancel_a.clone()));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(a.sink.last(), Some(Some(counts(1, 0, 0))));

    // Leadership lost while a refresh is in flight: the result is discarded.
    a.registry.hang();
    tokio::time::sleep(REFRESH + Duration::from_millis(10)).await;
    cancel_a.cancel();
    task_a.await.expect("run returns");
    assert_eq!(a.sink.last(), Some(None));
    assert_eq!(a.sink.publications().len(), 2);

    // The new leader publishes from its own storage read.
    let b = Harness::new();
    b.seed(None).await;
    let cancel_b = CancellationToken::new();
    let task_b = tokio::spawn(b.refresher.clone().run(cancel_b.clone()));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(b.sink.last(), Some(Some(counts(0, 1, 0))));
    cancel_b.cancel();
    task_b.await.expect("run returns");
}

#[tokio::test]
async fn refresh_once_answers_cancellation_at_once() {
    let h = Harness::new();
    h.seed(Some(0)).await;
    h.registry.hang();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        h.refresh_with(&cancel).await.expect_err("cancelled"),
        RefreshError::Cancelled
    );
}

impl Harness {
    async fn refresh_with(
        &self,
        cancel: &CancellationToken,
    ) -> Result<LifecycleCounts, RefreshError> {
        self.refresher.refresh_once(cancel).await
    }
}
