//! Layer-1 tests for the lock (TESTING.md §2, `lock/mod.rs` row): key and
//! channel construction, the holder token, the `SET NX PX` argument assembly,
//! and the three-outcome classification of a failed `lock()`.
//!
//! Everything the acquisition loop *does* with those decisions needs a server
//! and is `RD-LOCK-001..013` at Layer 3. What is here is the part that can be
//! wrong without a server noticing: a channel the fan-out cannot parse back, a
//! TTL rounded to a value Redis reads as "delete this", or a timeout reported as
//! contention when Redis was in fact unreachable.

use super::*;
use std::collections::HashSet;

fn names() -> LockNames {
    LockNames::new("cluster")
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

#[test]
fn the_lease_key_carries_the_primitive_segment() {
    assert_eq!(
        names().lease_key("tenant-42/rate-limit"),
        "cluster:l:tenant-42/rate-limit"
    );
}

#[test]
fn the_release_channel_carries_its_own_segment() {
    assert_eq!(
        names().release_channel("tenant-42/rate-limit"),
        "cluster:e:l:tenant-42/rate-limit"
    );
}

#[test]
fn a_release_channel_round_trips_back_to_its_name() {
    // The property the wake path rests on: `lock_release` publishes to a channel
    // this type built, and the fan-out recovers the name with this function. A
    // mismatch would be a blocked `lock()` that silently never wakes.
    let names = names();
    for name in ["ledger", "tenant-42/rate-limit", "a:b:c", ""] {
        assert_eq!(
            names.name_from_release_channel(&names.release_channel(name)),
            Some(name.to_owned()),
            "the release channel for {name:?} must parse back to it"
        );
    }
}

#[test]
fn a_cache_event_channel_is_not_a_release_channel() {
    // The two families differ by one segment, and each is parsed only by the
    // type that builds it. `cache/watch_tests.rs` asserts the mirror image —
    // that `key_from_event_channel` returns `None` for a `:e:l:` channel — so
    // between them no message can be routed to the wrong registry.
    assert_eq!(
        names().name_from_release_channel("cluster:e:c:some-key"),
        None
    );
    assert_eq!(
        names().name_from_release_channel("cluster:l:some-lock"),
        None
    );
    assert_eq!(names().name_from_release_channel("other:e:l:ledger"), None);
}

#[test]
fn the_release_pattern_covers_this_prefix_and_escapes_it() {
    assert_eq!(names().release_pattern(), "cluster:e:l:*");
    // A `[` in an operator's `key_prefix` is a glob character class unescaped,
    // so the subscription would cover something other than what was asked for.
    assert_eq!(
        LockNames::new("cluster[a]").release_pattern(),
        r"cluster\[a\]:e:l:*"
    );
}

// ---------------------------------------------------------------------------
// The holder token
// ---------------------------------------------------------------------------

#[test]
fn every_acquisition_mints_a_fresh_token() {
    // The token is the entire fence behind `renew` and `release` (DESIGN.md
    // §5.2). Two acquisitions sharing one would let a lapsed holder renew or
    // release its successor's lease, which is the bug `RD-LOCK-006` exists for.
    let tokens: HashSet<String> = (0..1_000).map(|_| Uuid::new_v4().to_string()).collect();
    assert_eq!(tokens.len(), 1_000, "holder tokens must not repeat");
}

// ---------------------------------------------------------------------------
// SET NX PX / PEXPIRE argument assembly
// ---------------------------------------------------------------------------

#[test]
fn a_ttl_renders_as_whole_milliseconds() {
    assert_eq!(px_millis(Duration::from_secs(30)), 30_000);
    assert_eq!(px_millis(Duration::from_millis(1)), 1);
}

#[test]
fn a_sub_millisecond_ttl_rounds_up_rather_than_to_zero() {
    // `PX 0` is an error reply and `PEXPIRE k 0` deletes the key outright, so
    // rounding down would turn "expires almost immediately" into either a failed
    // acquisition or a lock released the instant it was taken.
    assert_eq!(px_millis(Duration::from_nanos(1)), 1);
    assert_eq!(px_millis(Duration::ZERO), 1);
}

#[test]
fn an_absurd_ttl_saturates_rather_than_wrapping() {
    // Redis rejects it, which is the honest outcome — what must not happen is a
    // wrap into a small or negative expiry, which would silently hand out a
    // lease far shorter than the caller asked for.
    assert_eq!(px_millis(Duration::MAX), i64::MAX);
}

// ---------------------------------------------------------------------------
// PTTL, and the two negative sentinels
// ---------------------------------------------------------------------------

#[test]
fn a_positive_pttl_is_the_lease_deadline() {
    assert_eq!(lease_remaining(40), Some(Duration::from_millis(40)));
}

#[test]
fn a_vanished_key_asks_for_an_immediate_retry() {
    // `-2` means the lease lapsed or was released between the `SET NX` that just
    // failed and this read, so the name is free now.
    assert_eq!(lease_remaining(-2), Some(Duration::ZERO));
}

#[test]
fn a_key_with_no_ttl_falls_back_to_the_heartbeat() {
    // `-1` is a key with no expiry, which is not a lease this plugin wrote —
    // every acquisition carries `PX`. There is no deadline to schedule against.
    assert_eq!(lease_remaining(-1), None);
}

// ---------------------------------------------------------------------------
// The three-outcome classification of a failed lock()
// ---------------------------------------------------------------------------

fn provider(kind: ProviderErrorKind) -> ClusterError {
    ClusterError::Provider {
        kind,
        message: "test".to_owned(),
    }
}

#[test]
fn an_unreachable_server_is_retried_inside_the_budget() {
    // `fred`'s reconnect is what carries a `lock()` through a Sentinel failover,
    // so a caller that asked for thirty seconds of patience gets it.
    for kind in [
        ProviderErrorKind::ConnectionLost,
        ProviderErrorKind::Timeout,
    ] {
        assert!(
            matches!(classify_attempt(provider(kind)), Attempt::Unreachable(_)),
            "{kind:?} must be waited out rather than ending the loop"
        );
    }
}

#[test]
fn a_shutdown_ends_the_loop_at_once() {
    // The distinction `RD-LOCK-012` measures: retrying a torn-down backend for a
    // 30 s budget and then reporting `LockTimeout` would leave a caller unable to
    // tell "someone else holds it" from "this backend is gone".
    assert!(matches!(
        classify_attempt(ClusterError::Shutdown),
        Attempt::Fatal(ClusterError::Shutdown)
    ));
}

#[test]
fn errors_that_waiting_cannot_fix_end_the_loop_at_once() {
    // Each of these is either the caller's answer already or a condition no
    // amount of patience clears; retrying would spend the whole budget to report
    // the same thing later.
    let fatal = [
        provider(ProviderErrorKind::AuthFailure),
        provider(ProviderErrorKind::ResourceExhausted),
        provider(ProviderErrorKind::Other),
        ClusterError::InvalidConfig {
            reason: "test".to_owned(),
        },
        ClusterError::Unsupported { feature: "test" },
    ];
    for err in fatal {
        assert!(
            matches!(classify_attempt(err), Attempt::Fatal(_)),
            "an error waiting cannot fix must end the loop"
        );
    }
}

#[test]
fn a_budget_spent_against_a_contended_lock_is_a_lock_timeout() {
    let waited = Duration::from_millis(750);
    let failure = lock_failure("ledger", waited, None);
    assert!(
        matches!(
            failure,
            ClusterError::LockTimeout { ref name, waited: reported }
                if name == "ledger" && reported == waited
        ),
        "genuine contention must report LockTimeout carrying what was waited, got {failure:?}"
    );
}

#[test]
fn a_budget_spent_against_an_unreachable_server_reports_the_outage() {
    // The half that matters operationally: `LockTimeout` says "back off and try
    // later", a retained `Provider` says "your Redis is down". Collapsing the
    // second into the first is what a loop that discarded its last error would
    // do.
    let failure = lock_failure(
        "ledger",
        Duration::from_secs(30),
        Some(provider(ProviderErrorKind::ConnectionLost)),
    );
    assert!(
        matches!(
            failure,
            ClusterError::Provider {
                kind: ProviderErrorKind::ConnectionLost,
                ..
            }
        ),
        "an outage that lasted the whole budget must not be reported as contention"
    );
}

// ---------------------------------------------------------------------------
// The ADR-004 lock signal set (DESIGN.md §9).
//
// The lock emits natively rather than through a decorator, so "which signal
// fires for which outcome" is this file's question rather than the SDK's. A
// recording `ClusterMetrics` answers it with no server: the two outcomes below
// are both reached before a single command is issued, which is exactly why they
// are the ones a Layer 1 test can hold.
// ---------------------------------------------------------------------------

/// A lock over a pool that is built but never connected.
///
/// `fred` only dials on `init()`, so this is a real backend that simply has no
/// server behind it. Both tests below return before reaching the pool at all.
fn lock_backend(
    shutdown: &CancellationToken,
    signals: Arc<crate::observability::RedisSignals>,
) -> RedisLock {
    let pool = fred::types::Builder::default_centralized()
        .build_pool(1)
        .expect("a one-connection pool builds without connecting");
    RedisLock::new(LockInit {
        pool,
        scripts: Arc::new(crate::scripts::ScriptCache::default()),
        names: names(),
        linearizable: false,
        wait: WaitPolicy::Disabled,
        waiters: ReleaseWaiters::new(),
        shutdown: shutdown.clone(),
        signals,
    })
}

#[tokio::test]
async fn a_shutdown_try_lock_is_recorded_as_shutdown_and_not_as_an_error() {
    // The trap `result::label` exists to close: `Shutdown` is a bounded outcome
    // of its own, so a graceful cluster shutdown must not spike
    // `cluster_provider_errors_total` once per in-flight acquisition.
    let (signals, recorder) = crate::test_support::recording_signals();
    let shutdown = CancellationToken::new();
    let lock = lock_backend(&shutdown, signals);
    shutdown.cancel();

    let outcome = lock.try_lock("ledger", Duration::from_secs(10)).await;
    assert!(matches!(outcome, Err(ClusterError::Shutdown)));
    assert_eq!(
        recorder.lock_ops(),
        vec![("try_lock".to_owned(), "shutdown".to_owned())]
    );
    assert!(
        recorder.provider_error_kinds().is_empty(),
        "a shutdown is an outcome, not a backend fault"
    );
}

#[tokio::test]
async fn a_shutdown_blocking_lock_is_recorded_under_the_lock_op() {
    // `lock` and `try_lock` are separate `op` label values because they are
    // separate operations with wildly different latency distributions: folding
    // a 30 s blocked acquisition into `try_lock`'s histogram would make the
    // non-blocking op look pathological.
    let (signals, recorder) = crate::test_support::recording_signals();
    let shutdown = CancellationToken::new();
    let lock = lock_backend(&shutdown, signals);
    shutdown.cancel();

    let outcome = lock
        .lock("ledger", Duration::from_secs(10), Duration::from_secs(30))
        .await;
    assert!(matches!(outcome, Err(ClusterError::Shutdown)));
    assert_eq!(
        recorder.lock_ops(),
        vec![("lock".to_owned(), "shutdown".to_owned())]
    );
    assert!(recorder.provider_error_kinds().is_empty());
}
// ---------------------------------------------------------------------------
// The third bound (DESIGN.md §5.3): the caller's `timeout` bounds the whole
// `lock()` call, including the reads inside it.
// ---------------------------------------------------------------------------

/// `park` returns on the caller's budget even when the `PTTL` that sizes its
/// sleep never answers.
///
/// The overrun this closes: `park` reads `PTTL` to size the sleep against the
/// lease actually blocking the caller, and that round trip used to carry no
/// bound but `fred`'s 5 s default command timeout. A Redis that stalls at that
/// moment — a `BGSAVE` fork pause, a slow `SCAN` from another tenant — turns a
/// 50 ms budget into a 5 s one, and `lock()` reports `LockTimeout` seconds late
/// to a caller measuring its own budget. The acquisition attempt on the same
/// loop was already wrapped; this is the read between the two.
///
/// A pool that is built but never `init()`ed is what makes this a Layer-1 test:
/// `fred` only dials on `init()`, so the command is queued against a router that
/// does not exist and the future is simply pending forever — the stalled server,
/// without a server. `tokio::time::pause()` then supplies the clock, so the
/// assertion is on virtual time and the test costs no wall clock.
#[tokio::test(start_paused = true)]
async fn park_returns_on_the_callers_budget_when_the_pttl_read_stalls() {
    const BUDGET: Duration = Duration::from_millis(50);

    let shutdown = CancellationToken::new();
    let (signals, _recorder) = crate::test_support::recording_signals();
    let lock = lock_backend(&shutdown, signals);
    let waiters = ReleaseWaiters::new();
    let released = waiters.wait_for("ledger");

    let started = tokio::time::Instant::now();
    lock.park("ledger", released, started + BUDGET)
        .await
        .expect("park returns Ok when it is the budget rather than shutdown that ends the wait");
    let elapsed = started.elapsed();

    assert!(
        elapsed <= BUDGET,
        "park must not outlive the budget it was given. Unbounded, the PTTL read is capped only \
         by fred's 5 s default command timeout, so this returns two orders of magnitude late \
         (budget {BUDGET:?}, elapsed {elapsed:?})"
    );
}
