//! The `stop()` join bound and the `Drop` diagnostic guard shared by every plugin
//! handle (DESIGN.md §11), modelled on `postgres-cluster-plugin/src/shutdown.rs`.
//!
//! This plugin's backends each own their own `CancellationToken`, background tasks,
//! and `async fn stop(&self)` (see `leader`/`lock`/`cache`), so a handle *delegates*
//! shutdown rather than holding one shared token: `stop()` awaits each backend's
//! `stop()`, and `Drop` calls each backend's synchronous `cancel()`. What is common
//! to all four handles — the combined one and the three per-primitive ones — is the
//! two rules encoded here, factored out so neither can drift between handles:
//!
//! * [`TASK_JOIN_TIMEOUT`] bounds the delegated joins, so an unresponsive watch
//!   stream cannot hold `stop()` open past a supervisor's shutdown budget.
//! * [`diagnose_drop`] is the `Drop` backstop for a `stop()` that never completed:
//!   it **cancels first** (so a forgotten handle still tears its tasks down) and
//!   then reports what the caller should say about it.

use std::time::Duration;

use tracing::warn;

/// Upper bound on the delegated backend joins in a handle's `stop()` (DESIGN.md
/// §11).
///
/// Each backend's `stop()` cancels its token and awaits its tasks. Those tasks are
/// `select!`ed on the cancel and exit promptly, but a task parked mid–`await` in a
/// watch stream's body cannot be preempted by the cancel branch, and `kube` applies
/// no read timeout to a long-lived watch. Unbounded, that would relocate the
/// shutdown stall out of the join DESIGN.md §11 tells operators to budget for and
/// into a step with no budget at all.
///
/// Ten seconds: long enough for any healthy in-flight request (the per-request
/// budget defaults to 10 s) to finish, short enough to stay inside a typical
/// supervisor's shutdown budget. On elapse the handle logs and proceeds — the tokens
/// are already cancelled, so the stragglers exit on their own; what is lost is only
/// the guarantee that every task is gone by the time `stop()` returns.
pub const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Awaits `join` under [`TASK_JOIN_TIMEOUT`], logging (rather than blocking `stop()`
/// indefinitely) if the budget elapses.
///
/// `join` is the composite future that stops every backend the handle owns; running
/// them under one shared deadline keeps the whole delegated teardown inside the one
/// budget DESIGN.md §11 documents.
pub async fn join_backends<F>(join: F)
where
    F: std::future::Future<Output = ()>,
{
    if tokio::time::timeout(TASK_JOIN_TIMEOUT, join).await.is_err() {
        warn!(
            timeout_ms = u64::try_from(TASK_JOIN_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            "cluster.provider.task_join_timeout: a backend task was still running at shutdown; its \
             token is cancelled and it will exit on its own, but at least one task outlives stop()"
        );
    }
}

/// What a handle's `Drop` should say about itself, once [`diagnose_drop`] has done
/// the cancel that must happen regardless.
#[derive(Debug, PartialEq, Eq)]
pub enum DropDiagnosis {
    /// `stop()` ran to completion. Nothing to cancel and nothing to report.
    StoppedCleanly,
    /// Dropped while a panic was already unwinding. The caller should warn and
    /// **not** panic: a second panic during unwind aborts the process, replacing a
    /// legible test failure with a bare `SIGABRT`.
    DuringPanic,
    /// Dropped without `stop()` outside a panic — the case worth shouting about
    /// (panic in debug, warn in release, per ADR-006 Confirmation).
    Unstopped,
}

/// Runs `cancel` unless the handle stopped cleanly, then reports what the caller
/// should do about it.
///
/// **The cancel comes first and unconditionally, and that ordering is the whole
/// point of this function** (mirroring postgres `cancel_and_diagnose_drop`). Reaching
/// a handle's `Drop` with `stopped == false` covers two cases, and the second is the
/// one that matters operationally:
///
/// 1. a handle genuinely forgotten — the programming error the diagnosis exists to
///    shout about; and
/// 2. a `stop()` whose *future was dropped part-way*, which is exactly what
///    `tokio::time::timeout(D, handle.stop())` does when its budget elapses — the
///    supervisor-level pattern DESIGN.md §11 recommends.
///
/// In both, cancelling is what lets each backend's watcher/renewal/reaper tasks
/// observe shutdown and exit instead of running forever against a handle nobody
/// owns. In case 2 the handle is not misused at all, so a diagnosis that ran
/// *instead of* cancelling would punish the caller for following the documented
/// advice.
///
/// `cancel` must also precede the caller's `#[cfg(debug_assertions)] panic!`, or the
/// cancel would be unreachable in debug builds — which is why this returns a
/// [`DropDiagnosis`] rather than emitting the diagnosis itself, exactly as the
/// postgres original does.
pub fn diagnose_drop(stopped: bool, cancel: impl FnOnce()) -> DropDiagnosis {
    if stopped {
        return DropDiagnosis::StoppedCleanly;
    }
    cancel();
    if std::thread::panicking() {
        return DropDiagnosis::DuringPanic;
    }
    DropDiagnosis::Unstopped
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{DropDiagnosis, diagnose_drop};

    /// The regression this guard exists for: a handle dropped without `stop()` must
    /// have cancelled its backends *before* the caller reaches its diagnosis, so the
    /// background tasks unwind.
    #[test]
    fn dropping_without_stop_cancels_before_diagnosing() {
        let cancelled = Cell::new(false);
        let diagnosis = diagnose_drop(false, || cancelled.set(true));
        assert_eq!(diagnosis, DropDiagnosis::Unstopped);
        assert!(
            cancelled.get(),
            "the backends must already be cancelled by the time the caller diagnoses, or the \
             debug-build panic makes the cancel unreachable"
        );
    }

    /// A clean `stop()` has already cancelled every backend, so `Drop` must stay
    /// silent and must not cancel again.
    #[test]
    fn a_cleanly_stopped_handle_is_not_diagnosed_and_does_not_cancel() {
        let cancelled = Cell::new(false);
        let diagnosis = diagnose_drop(true, || cancelled.set(true));
        assert_eq!(diagnosis, DropDiagnosis::StoppedCleanly);
        assert!(
            !cancelled.get(),
            "stop() owns the cancel on the clean path; Drop must not claim it too"
        );
    }

    /// Cancelling is unconditional, but the *diagnosis* is not: during a panic
    /// unwind the caller must warn instead of panicking. The cancel still has to run.
    #[test]
    fn dropping_during_a_panic_still_cancels_but_asks_for_a_warning() {
        // `std::thread::panicking()` is only true inside an unwinding drop, so drive
        // the real thing rather than faking the predicate.
        struct Dropper {
            cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
            diagnosis: std::sync::Arc<std::sync::Mutex<Option<DropDiagnosis>>>,
        }
        impl Drop for Dropper {
            fn drop(&mut self) {
                let cancelled = std::sync::Arc::clone(&self.cancelled);
                let diagnosis = diagnose_drop(false, move || {
                    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                });
                *self.diagnosis.lock().expect("uncontended") = Some(diagnosis);
            }
        }

        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let diagnosis = std::sync::Arc::new(std::sync::Mutex::new(None));
        let panicked = std::panic::catch_unwind({
            let cancelled = std::sync::Arc::clone(&cancelled);
            let diagnosis = std::sync::Arc::clone(&diagnosis);
            move || {
                let _dropper = Dropper {
                    cancelled,
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
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "cancelling is unconditional: a panicking shutdown still has to unwind its tasks"
        );
    }
}
