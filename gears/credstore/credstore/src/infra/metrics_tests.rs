//! Unit tests for the OpenTelemetry-backed [`CredStoreMetricsMeter`].

#[cfg(feature = "test-support")]
use super::test_harness::MetricsHarness;
#[cfg(feature = "test-support")]
use crate::domain::ports::metrics::{
    CredStoreMetricsPort, Dep, DepOp, FenceVerify, Outcome, ReadOutcome,
};

/// Smoke test that exercises instrument construction and every recording
/// path against the global (no-op) meter — no SDK exporter required, so it
/// runs under the default feature set (unlike the `test-support` tests).
#[test]
fn global_meter_records_all_instruments() {
    use super::CredStoreMetricsMeter;
    use crate::domain::ports::metrics::{CredStoreMetricsPort, Dep, DepOp, Outcome, ReadOutcome};

    use crate::domain::ports::metrics::FenceVerify;

    let m = CredStoreMetricsMeter::from_global();
    assert!(!format!("{m:?}").is_empty());

    for outcome in [
        ReadOutcome::HitOwn,
        ReadOutcome::HitInherited,
        ReadOutcome::Miss,
    ] {
        m.read_outcome(outcome);
    }
    m.walkup_depth(2);
    m.dependency(Dep::Plugin, DepOp::PluginGet, Outcome::Success, 0.01);
    m.dependency(Dep::Pdp, DepOp::Evaluate, Outcome::Error, 0.02);
    m.cross_tenant_denied();
    m.fence_verify(FenceVerify::Ok);
    m.fence_verify(FenceVerify::Mismatch);
    m.gc_deleted(3);
    m.gc_pending_reclaimed(1);
    m.expired_deleted(2);
}

#[test]
#[cfg(feature = "test-support")]
fn read_outcome_emits_with_label() {
    let h = MetricsHarness::new();
    let m = h.metrics();
    m.read_outcome(ReadOutcome::HitInherited);
    m.read_outcome(ReadOutcome::Miss);
    h.force_flush();
    assert_eq!(
        h.counter_value(
            "credstore_read_outcome_total",
            &[("outcome", "hit_inherited")]
        ),
        1
    );
    assert_eq!(
        h.counter_value("credstore_read_outcome_total", &[("outcome", "miss")]),
        1
    );
}

#[test]
#[cfg(feature = "test-support")]
fn dependency_emits_duration_and_health() {
    let h = MetricsHarness::new();
    let m = h.metrics();
    m.dependency(Dep::Plugin, DepOp::PluginGet, Outcome::Success, 0.005);
    h.force_flush();
    assert_eq!(
        h.histogram_count(
            "credstore_dependency_query_duration_seconds",
            &[("dependency", "plugin"), ("operation", "plugin_get")]
        ),
        1
    );
    assert_eq!(
        h.counter_value(
            "credstore_dependency_health_total",
            &[
                ("dependency", "plugin"),
                ("operation", "plugin_get"),
                ("outcome", "success")
            ]
        ),
        1
    );
}

#[test]
#[cfg(feature = "test-support")]
fn fence_verify_emits_with_outcome_labels() {
    let h = MetricsHarness::new();
    let m = h.metrics();
    m.fence_verify(FenceVerify::Ok);
    m.fence_verify(FenceVerify::Mismatch);
    m.fence_verify(FenceVerify::Mismatch);
    h.force_flush();
    assert_eq!(
        h.counter_value("credstore_fence_verify_total", &[("outcome", "ok")]),
        1
    );
    assert_eq!(
        h.counter_value("credstore_fence_verify_total", &[("outcome", "mismatch")]),
        2
    );
}

#[test]
#[cfg(feature = "test-support")]
fn gc_counters_accumulate_independently() {
    let h = MetricsHarness::new();
    let m = h.metrics();
    m.gc_deleted(3);
    m.gc_deleted(2);
    m.gc_pending_reclaimed(1);
    m.expired_deleted(4);
    h.force_flush();
    assert_eq!(h.counter_value("credstore_gc_deleted_total", &[]), 5);
    assert_eq!(
        h.counter_value("credstore_gc_pending_reclaimed_total", &[]),
        1
    );
    assert_eq!(h.counter_value("credstore_expired_deleted_total", &[]), 4);
}
