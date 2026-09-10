//! Shutdown machinery shared by both plugin handles — `RedisClusterHandle`
//! (cache + lock) and `RedisLockHandle` (lock only) — DESIGN.md §11.
//!
//! One file rather than a copy per handle, and DESIGN.md §3.1 records why. The
//! obvious alternative — the ADR-006 `Drop` guard in `plugin.rs`, the bounded
//! pool close in `lock/mod.rs` — is what the Postgres plugin does, and its two
//! copies drifted in both directions: one handle bounded its pool close and the
//! other did not, one cancelled the shutdown token in `Drop` and the other did
//! not, so a `stop()` future dropped by a supervisor timeout leaked every
//! background task and the pool with it while the design document asserted the
//! opposite. This plugin has the identical two-handle shape, so it holds one
//! implementation of each rule and neither handle can drift from a rule it does
//! not own.

use std::time::Duration;

use fred::clients::{Pool, SubscriberClient};
use fred::interfaces::ClientLike;
use fred::types::ConnectHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Upper bound on the final pool `quit()` in either handle's `stop()`
/// (DESIGN.md §11 step 3).
///
/// `fred`'s `quit` drains in-flight commands before closing each connection,
/// which is the behaviour worth having — a `stop()` that severed the socket
/// mid-command would turn an orderly shutdown into a spurious `ConnectionLost`
/// for whatever was in flight. The bound exists because that drain is only as
/// fast as the server: against an unresponsive Redis the drain is exactly as
/// long as the server is unresponsive, and a supervisor's shutdown budget must
/// not be spent there.
///
/// Ten seconds: comfortably longer than the 5 s default `command_timeout_ms`,
/// so every healthy in-flight command either completes or times out well inside
/// it, and short enough to stay within a typical supervisor budget. Giving up
/// on the wait still leaves the client shut down — what is lost is only the
/// guarantee that the server has seen the `QUIT`, which an unresponsive peer was
/// never going to give.
pub const POOL_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on draining a handle's tracked background tasks in `stop()`,
/// before the pool it shares with them is closed.
///
/// The per-guard lock tasks (DESIGN.md §5) are the reason this exists. Each selects on the shutdown token, so the only
/// thing that can delay one is a `renew` or `release` already in flight — which
/// `command_timeout_ms` bounds client-side. Five seconds is therefore generous
/// rather than tight: it is the default command timeout, which is the longest
/// any single in-flight command can take, and it keeps the drain well inside the
/// pool close that follows it.
pub const GUARD_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Closes `tracker` to new tasks and waits for the running ones under
/// [`GUARD_DRAIN_TIMEOUT`].
///
/// Tracking rather than detaching is what makes this possible at all: a
/// `tokio::spawn`ed guard task is invisible to `stop()`, which could then return
/// while tasks it started were still holding pool clones — the exact leak the
/// module docs above describe the Postgres plugin suffering. `what` names the
/// task family in the timeout warning, so an operator reading it knows which
/// one did not finish.
pub async fn drain_tracked_tasks(tracker: &tokio_util::task::TaskTracker, what: &str) {
    tracker.close();
    if tokio::time::timeout(GUARD_DRAIN_TIMEOUT, tracker.wait())
        .await
        .is_err()
    {
        warn!(
            name: crate::observability::logs::TASK_DRAIN_TIMEOUT,
            what,
            timeout_ms = u64::try_from(GUARD_DRAIN_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            "cluster.provider.task_drain_timeout: background tasks did not finish within the \
             shutdown budget; the pool is closed either way, but at least one task may outlive \
             stop()"
        );
    }
}

/// Quits `pool` under [`POOL_CLOSE_TIMEOUT`], logging if the budget elapses
/// rather than blocking `stop()` indefinitely.
pub async fn close_pool(pool: &Pool) {
    match tokio::time::timeout(POOL_CLOSE_TIMEOUT, pool.quit()).await {
        Ok(Ok(())) => {}
        // A `quit` that fails is ordinary rather than alarming — the commonest
        // cause is the connection already being gone, which is the state `quit`
        // was trying to reach. DEBUG so it is available when reconstructing a
        // shutdown, without making every restart against a bounced server look
        // like a problem.
        Ok(Err(err)) => {
            tracing::debug!(error = %err, "redis pool quit reported an error while shutting down");
        }
        Err(_elapsed) => warn!(
            name: crate::observability::logs::POOL_CLOSE_TIMEOUT,
            timeout_ms = u64::try_from(POOL_CLOSE_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            "cluster.provider.pool_close_timeout: the redis command pool did not finish draining \
             within the shutdown budget; the client is shut down either way, but at least one \
             connection may outlive stop()"
        ),
    }
}

/// Tears down a subscriber that was connected but never subscribed — the
/// startup steps between `connect()` and `start_subscriber` (DESIGN.md §3.2
/// step 6).
///
/// **Dropping the client closes nothing.** `fred` 10.1.0's
/// `impl Drop for ClientInner` (`src/modules/inner.rs:484`) sits behind
/// `#[cfg(feature = "credential-provider")]`, which nothing in this tree
/// enables, so in this build the client has no `Drop` impl at all — and even
/// under that feature its body only aborts the credential-refresh task, with a
/// `TODO` where the router quit would go. The router task spawned by `init()`
/// holds its own reference and keeps the socket open under the reconnect
/// policy for the life of the process; dropping the [`ConnectHandle`] detaches
/// its task rather than ending it. An error path that returns without calling
/// this leaks one connection and one task per attempt, which a supervisor
/// retrying gear boot accumulates indefinitely.
///
/// The order mirrors `start_subscriber`'s own failure paths: abort the
/// connection task, then `QUIT`. There is no `manager` to abort at these
/// earlier points, because `manage_subscriptions()` has not been called yet.
pub async fn abandon_subscriber(client: &SubscriberClient, connection: &ConnectHandle) {
    connection.abort();
    crate::subscriber::quit_subscriber(client).await;
}

/// What a handle's `Drop` should say about itself, once
/// [`cancel_and_diagnose_drop`] has done the part that must happen regardless.
#[derive(Debug, PartialEq, Eq)]
pub enum DropDiagnosis {
    /// `stop()` ran to completion. Nothing to cancel and nothing to report.
    StoppedCleanly,
    /// Dropped while a panic was already unwinding. The caller should warn and
    /// **not** panic: a second panic during unwind aborts the process, which
    /// would replace a legible test failure with a bare `SIGABRT`.
    DuringPanic,
    /// Dropped without `stop()` outside a panic — the case worth shouting about
    /// (panic in debug, warn in release, per ADR-006 §Confirmation).
    Unstopped,
}

/// Cancels `shutdown` unless the handle stopped cleanly, then reports what the
/// caller should do about it.
///
/// **The cancel comes first and unconditionally, and that ordering is the whole
/// point of this function.** Reaching a handle's `Drop` with `stopped == false`
/// covers two cases, and the second is the one that matters operationally:
///
/// 1. a handle genuinely forgotten — the programming error the diagnosis exists
///    to shout about; and
/// 2. a `stop()` whose *future was dropped part-way*, which is exactly what
///    `tokio::time::timeout(d, handle.stop())` does when its budget elapses —
///    the supervisor-level pattern DESIGN.md §11 recommends.
///
/// In both, cancelling is what lets the subscriber fan-out task, the reconnect
/// observer, and the per-guard lock tasks observe shutdown and exit, instead of
/// running forever against a handle nobody owns while each
/// still holds a pool clone that consequently never closes. In case 2 the handle
/// is not misused at all, so a diagnosis that ran *instead of* cancelling would
/// punish the caller for following the documented advice.
///
/// It must also precede the caller's `#[cfg(debug_assertions)] panic!`, or the
/// cancel would be unreachable in debug builds — which is why this returns a
/// [`DropDiagnosis`] rather than emitting the diagnosis itself. The panic is
/// compiled in exactly the configuration tests run in, so "the token was
/// cancelled on the path that would otherwise panic" is not observable from a
/// test that lets the `Drop` impl run to completion. Returning the decision
/// makes that ordering directly assertable.
#[must_use]
pub fn cancel_and_diagnose_drop(stopped: bool, shutdown: &CancellationToken) -> DropDiagnosis {
    if stopped {
        return DropDiagnosis::StoppedCleanly;
    }
    shutdown.cancel();
    if std::thread::panicking() {
        return DropDiagnosis::DuringPanic;
    }
    DropDiagnosis::Unstopped
}

// Layer-1 unit tests: the `Drop`-ordering rule this module exists to hold.
// Out-of-line per DE1101.
#[cfg(test)]
#[path = "shutdown_tests.rs"]
mod tests;
