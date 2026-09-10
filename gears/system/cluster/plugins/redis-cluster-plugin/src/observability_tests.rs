//! Layer-1 unit tests for the observability surface (TESTING.md §2): the
//! eviction rate limiter's pure policy, the `_total` naming rule, and the
//! provider-error routing rule the whole plugin depends on.

use std::time::Duration;

use cluster_sdk::observability::{ResourceId, kind, metrics as catalog};
use cluster_sdk::{ClusterError, ProviderErrorKind};

use super::*;
use crate::test_support::{metered_signals, recording_signals};

#[test]
fn the_first_eviction_always_reports() {
    // A rate limiter that started "closed" would swallow the single most
    // valuable line this plugin emits on a deployment that evicts once.
    let reporter = EvictionReporter::new(Duration::from_secs(30));
    assert_eq!(reporter.claim(0), Some(0));
}

#[test]
fn evictions_inside_the_window_are_suppressed_and_counted_onto_the_next_line() {
    let reporter = EvictionReporter::new(Duration::from_secs(30));
    assert_eq!(reporter.claim(0), Some(0));
    assert_eq!(reporter.claim(1), None);
    assert_eq!(reporter.claim(29_999), None);
    // The burst is reported rather than lost: the two suppressed evictions ride
    // the first line the window allows through.
    assert_eq!(reporter.claim(30_000), Some(2));
    // And the count resets, so the next line does not re-report them.
    assert_eq!(reporter.claim(60_000), Some(0));
}

#[test]
fn a_clock_that_does_not_advance_still_terminates() {
    // `saturating_sub` rather than a subtraction: a reading that goes backwards
    // (or stays put) must suppress, never panic and never divide by a window.
    let reporter = EvictionReporter::new(Duration::from_secs(30));
    assert_eq!(reporter.claim(1_000), Some(0));
    assert_eq!(reporter.claim(0), None);
}

#[test]
fn counter_instruments_drop_the_contract_total_suffix() {
    // The exporter re-appends it, so an instrument created with the suffix
    // already on it scrapes as `..._total_total`. The catalog constant stays the
    // name a dashboard queries.
    assert_eq!(
        counter_name(WATCH_EVENTS_DROPPED),
        "cluster_redis_watch_events_dropped"
    );
    assert_eq!(
        counter_name(catalog::PROVIDER_ERRORS_TOTAL),
        "cluster_provider_errors"
    );
    // A gauge carries no `_total` to strip and is used verbatim.
    assert_eq!(counter_name(CONNECTION_STATE), CONNECTION_STATE);
}

#[test]
fn the_plugin_local_metric_names_are_the_ones_design_9_lists() {
    // Pinned as literals rather than derived, because these four names are the
    // contract: a rename is a dashboard break, and it should look like one in
    // the diff.
    assert_eq!(
        WATCH_EVENTS_DROPPED,
        "cluster_redis_watch_events_dropped_total"
    );
    assert_eq!(
        SUBSCRIBER_RESUBSCRIBES,
        "cluster_redis_subscriber_resubscribes_total"
    );
    assert_eq!(SCRIPT_RELOADS, "cluster_redis_script_reloads_total");
    assert_eq!(CONNECTION_STATE, "cluster_redis_connection_state");
    // The plugin-local counter DESIGN.md §9 names, and the replacement
    // for the unemittable `cluster_provider_errors_total{op="eviction"}`.
    assert_eq!(EVICTIONS_OBSERVED, "cluster_redis_evictions_observed_total");
}

#[test]
fn the_subscriber_lost_event_name_is_the_one_design_9_lists() {
    // Pinned for the same reason as the metric names above: this is the line
    // announcing that the subscriber is permanently gone, so it is the one an
    // alert filters structurally on, and a rename should look like a broken
    // alert in the diff.
    //
    // In the `cluster.provider.*` family deliberately — the condition also costs
    // a blocked `lock()` its release wake, and the watchdog that emits it runs
    // with no registry at all under `watch_mode: disabled` and in the standalone
    // lock plugin.
    assert_eq!(
        crate::observability::logs::SUBSCRIBER_LOST,
        "cluster.provider.subscriber_lost"
    );
}

#[test]
fn a_normal_outcome_is_never_counted_as_a_provider_error() {
    // The rule the whole plugin rests on: contention, a CAS conflict, a lock
    // timeout, an expired lease, and a shutdown are answers, not faults. Every
    // one of them reaching `cluster_provider_errors_total` would make the error
    // rate track lock contention.
    let (signals, recorder) = recording_signals();
    for err in [
        ClusterError::LockContended {
            name: "n".to_owned(),
        },
        ClusterError::LockTimeout {
            name: "n".to_owned(),
            waited: Duration::from_secs(1),
        },
        ClusterError::LockExpired {
            name: "n".to_owned(),
        },
        ClusterError::CasConflict {
            key: "k".to_owned(),
            current: None,
        },
        ClusterError::Shutdown,
        ClusterError::Unsupported { feature: "watch" },
    ] {
        signals.provider_error("try_lock", ResourceId::Lock("n"), &err);
    }
    assert!(
        recorder.provider_error_kinds().is_empty(),
        "normal outcomes must not reach the provider-error counter, got {:?}",
        recorder.provider_error_kinds()
    );
}

#[test]
fn a_backend_failure_is_counted_under_its_bounded_kind() {
    let (signals, recorder) = recording_signals();
    signals.provider_error(
        "renew",
        ResourceId::Lock("n"),
        &ClusterError::Provider {
            kind: ProviderErrorKind::ConnectionLost,
            message: "gone".to_owned(),
        },
    );
    assert_eq!(recorder.provider_error_kinds(), vec![kind::CONNECTION_LOST]);
}

#[test]
fn every_eviction_is_counted_even_though_only_some_are_logged() {
    // The half of the rate limiter that is easy to get wrong: throttling the
    // WARN must not throttle the counter, or an alert on an eviction storm would
    // see one eviction per window and fire on none of them.
    let (signals, readback) = metered_signals();
    for index in 0..25 {
        signals.eviction_observed(Primitive::Cache, &format!("tenant-{index}/key"));
    }
    assert_eq!(readback.counter(EVICTIONS_OBSERVED), 25);
}

#[test]
fn an_evicted_lease_and_an_evicted_entry_are_both_counted() {
    // Both primitives land on the same counter under their own `primitive`
    // label, because an alert has to be able to tell a re-read from a
    // double-held lock. A counter fed only by the cache would miss the case
    // DESIGN.md §3.7 opens with and rates worst.
    let (signals, readback) = metered_signals();
    signals.eviction_observed(Primitive::Cache, "tenant-1/entry");
    signals.eviction_observed(Primitive::Lock, "tenant-1/leader");
    assert_eq!(readback.counter(EVICTIONS_OBSERVED), 2);
}

#[test]
fn an_eviction_is_not_counted_as_a_provider_error() {
    // DESIGN.md §3.7 and `RD-SPEC-007` both
    // ask for `cluster_provider_errors_total{op="eviction"}`, which has no `op`
    // label to carry it and no `ClusterError` to travel through. The counter
    // above is the replacement, and this pins that the catalog counter was not
    // quietly conscripted for it instead.
    let (signals, recorder) = recording_signals();
    signals.eviction_observed(Primitive::Cache, "tenant-1/key");
    assert!(
        recorder.provider_error_kinds().is_empty(),
        "an eviction is not an operation failure and must not reach the error counter"
    );
}

#[test]
fn a_watch_reset_is_attributed_to_the_cache_primitive() {
    // `primitive` is a bounded label and the cache is the only primitive with a
    // watch here: the lock's release wake is not a watch and must not inflate
    // this series.
    let (signals, recorder) = recording_signals();
    signals.watch_reset();
    assert_eq!(recorder.watch_resets(), vec!["cache"]);
}
