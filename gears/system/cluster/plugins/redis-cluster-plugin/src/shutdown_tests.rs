use super::*;

/// The regression this function exists for: a handle dropped without `stop()`
/// must have cancelled the shared token *before* the caller reaches its
/// diagnosis, so the background tasks unwind. The Postgres plugin's combined
/// handle shipped without this and leaked both reapers and both `LISTEN` tasks —
/// each pinning a pool clone, so the pool never closed either — for the life of
/// the process.
#[test]
fn dropping_without_stop_cancels_before_diagnosing() {
    let shutdown = CancellationToken::new();

    let diagnosis = cancel_and_diagnose_drop(false, &shutdown);

    assert_eq!(
        diagnosis,
        DropDiagnosis::Unstopped,
        "a handle dropped without stop() outside a panic must be reported"
    );
    assert!(
        shutdown.is_cancelled(),
        "the token must already be cancelled by the time the caller diagnoses, or the debug-build \
         panic makes the cancel unreachable"
    );
}

/// A clean `stop()` has already cancelled the token itself, and the handle
/// carries nothing left to unwind — so `Drop` must stay silent rather than
/// re-reporting a shutdown that worked.
#[test]
fn a_cleanly_stopped_handle_is_not_diagnosed() {
    let shutdown = CancellationToken::new();

    assert_eq!(
        cancel_and_diagnose_drop(true, &shutdown),
        DropDiagnosis::StoppedCleanly
    );
}

/// `stopped == true` must not cancel on its own account. The flag means `stop()`
/// completed, which already cancelled the token; cancelling here would be
/// harmless but would make this function's contract ("cancels unless the handle
/// stopped cleanly") false, and the next reader would have to re-derive which of
/// the two actually holds.
#[test]
fn a_cleanly_stopped_handle_does_not_cancel_here() {
    let shutdown = CancellationToken::new();

    let _diagnosis = cancel_and_diagnose_drop(true, &shutdown);

    assert!(
        !shutdown.is_cancelled(),
        "stop() owns the cancel on the clean path; Drop must not claim it too"
    );
}

/// Cancelling is unconditional, but the *diagnosis* is not: during a panic
/// unwind the caller must warn instead of panicking, since a second panic
/// aborts. The token still has to be cancelled — a test failing mid-critical
/// section is precisely when the background tasks most need to be told.
#[test]
fn dropping_during_a_panic_still_cancels_but_asks_for_a_warning() {
    // `std::thread::panicking()` is only true inside an unwinding drop, so drive
    // the real thing rather than faking the predicate.
    struct Dropper {
        shutdown: CancellationToken,
        diagnosis: std::sync::Arc<std::sync::Mutex<Option<DropDiagnosis>>>,
    }
    impl Drop for Dropper {
        fn drop(&mut self) {
            let diagnosis = cancel_and_diagnose_drop(false, &self.shutdown);
            *self.diagnosis.lock().expect("uncontended") = Some(diagnosis);
        }
    }

    let shutdown = CancellationToken::new();
    let diagnosis = std::sync::Arc::new(std::sync::Mutex::new(None));
    let panicked = std::panic::catch_unwind({
        let shutdown = shutdown.clone();
        let diagnosis = std::sync::Arc::clone(&diagnosis);
        move || {
            let _dropper = Dropper {
                shutdown,
                diagnosis,
            };
            panic!(
                "simulated failure mid-shutdown (expected: this test drives a real unwinding \
                 Drop, so this line on stderr is part of a passing run)"
            );
        }
    });

    assert!(panicked.is_err(), "setup: the closure must have panicked");
    assert_eq!(
        *diagnosis.lock().expect("uncontended"),
        Some(DropDiagnosis::DuringPanic),
        "a Drop running during unwind must ask for a warning, never a second panic"
    );
    assert!(
        shutdown.is_cancelled(),
        "cancelling is unconditional: a panicking shutdown still has to unwind its tasks"
    );
}

/// [`abandon_subscriber`] must actually stop the router task `connect()` spawned,
/// not merely drop a handle to it. Dropping the `SubscriberClient` closes nothing
/// in this build of `fred` (the function's own doc explains why), so a startup
/// that failed after the subscriber connected — `connect.rs`'s subscriber
/// timeout/error arms — would leak one connection and one router task per attempt
/// without this call. `RD-LIFE-010` proves the leak-free property for the startup
/// steps *after* `connect()`; this covers the teardown primitive those arms rely
/// on, at the unit layer, since forcing the end-to-end subscriber connect timeout
/// itself needs the fault-injection harness this crate does not build
/// (TESTING.md §5/§8).
///
/// Pointed at a closed port under the production reconnect policy on purpose: the
/// router task then stays *pending*, retrying (`RECONNECT_ATTEMPTS` = 20 with
/// exponential backoff runs for minutes), so its disappearance after teardown is
/// attributable to the abort rather than to the task exhausting its schedule on
/// its own. No server, so this stays a Layer 1 test.
#[tokio::test]
async fn abandon_subscriber_stops_the_router_task() {
    use fred::types::Builder;
    use fred::types::config::Config;

    let config = Config::from_url("redis://127.0.0.1:1").expect("a well-formed url parses");
    let mut builder = Builder::from_config(config);
    builder.set_policy(crate::connect::reconnect_policy());
    let client = builder
        .build_subscriber_client()
        .expect("the subscriber client builds");
    let connection = client.connect();
    assert!(
        !connection.is_finished(),
        "setup: the router task retries under the reconnect policy, so it is running before \
         teardown - otherwise the assertion below would pass on a task that ended by itself"
    );

    // Bounded on purpose: a teardown that blocked on an unresponsive client would
    // be the very hang `stop()` exists to avoid, so completing at all is part of
    // what is under test.
    tokio::time::timeout(
        Duration::from_secs(5),
        abandon_subscriber(&client, &connection),
    )
    .await
    .expect("abandon_subscriber must complete promptly, not block on an unresponsive client");

    // `abort()` schedules cancellation; awaiting the handle observes it land, and
    // `is_cancelled()` is what distinguishes "the abort stopped it" from "it would
    // have stopped anyway".
    let outcome = connection.await;
    assert!(
        outcome.map_or_else(|err| err.is_cancelled(), |_completed| false),
        "the router task must be gone after teardown - aborted, not left retrying the \
         reconnect schedule"
    );
}
