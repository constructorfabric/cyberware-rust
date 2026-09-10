//! Layer-1 tests for the release-waiter registry and the wake-delay policy
//! (TESTING.md §2, `lock/waiters.rs` row). No server and no clock control: the
//! registry is pure in-process state, and the delay is a pure function of two
//! `Duration`s plus a draw.

use super::*;

// ---------------------------------------------------------------------------
// register / notify / deregister-on-drop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_notified_waiter_is_woken() {
    let waiters = ReleaseWaiters::new();
    let wait = waiters.wait_for("ledger");
    assert_eq!(waiters.registered("ledger"), 1);

    waiters.notify("ledger");

    assert!(wait.await.is_ok(), "a notified waiter must be woken");
    assert_eq!(
        waiters.registered("ledger"),
        0,
        "notifying must consume the registration rather than leave it behind"
    );
}

#[tokio::test]
async fn notifying_a_name_with_no_waiter_is_a_no_op() {
    // The common case on the fan-out path: every instance sees every release
    // published under this plugin's prefix, and almost none of them has a
    // blocked `lock()` for that name.
    let waiters = ReleaseWaiters::new();
    waiters.notify("nobody-is-waiting");
    assert_eq!(waiters.registered("nobody-is-waiting"), 0);
}

#[tokio::test]
async fn an_abandoned_wait_deregisters_itself_on_drop() {
    let waiters = ReleaseWaiters::new();
    {
        let _wait = waiters.wait_for("ledger");
        assert_eq!(waiters.registered("ledger"), 1);
    }
    assert_eq!(
        waiters.registered("ledger"),
        0,
        "a waiter that gave up must withdraw its own registration"
    );
}

#[tokio::test]
async fn repeated_waits_for_a_never_released_name_do_not_accumulate() {
    // The regression the per-waiter id exists for (PGR-M7, ported): the
    // acquisition loop calls `wait_for` once per attempt, so a name that is
    // renewed but never released would otherwise grow one dead sender per
    // heartbeat for the whole life of the waiter.
    let waiters = ReleaseWaiters::new();
    for _attempt in 0..100 {
        let _wait = waiters.wait_for("renewed-forever");
    }
    assert_eq!(waiters.registered("renewed-forever"), 0);
}

#[tokio::test]
async fn dropping_one_waiter_leaves_its_siblings_registered() {
    let waiters = ReleaseWaiters::new();
    let live = waiters.wait_for("ledger");
    {
        let _abandoned = waiters.wait_for("ledger");
        assert_eq!(waiters.registered("ledger"), 2);
    }
    assert_eq!(
        waiters.registered("ledger"),
        1,
        "withdrawing one waiter must not withdraw another"
    );

    waiters.notify("ledger");
    assert!(live.await.is_ok(), "the surviving waiter must still wake");
}

#[tokio::test]
async fn a_waiter_outliving_its_registry_resolves_rather_than_hanging() {
    // The registry is held by the lock backend, so this is what a waiter parked
    // across a handle teardown sees. It must not park forever: the caller's
    // next move is a `SET NX` against a pool that is closing, which answers.
    let waiters = ReleaseWaiters::new();
    let wait = waiters.wait_for("ledger");
    drop(waiters);
    assert!(
        wait.await.is_err(),
        "a dropped registry must resolve its waiters, not strand them"
    );
}

#[tokio::test]
async fn waiters_on_different_names_are_independent() {
    let waiters = ReleaseWaiters::new();
    let ledger = waiters.wait_for("ledger");
    let invoices = waiters.wait_for("invoices");

    waiters.notify("ledger");

    assert!(ledger.await.is_ok());
    assert_eq!(
        waiters.registered("invoices"),
        1,
        "a release of one name must not wake waiters on another"
    );
    drop(invoices);
}

// ---------------------------------------------------------------------------
// the wake delay: min(PTTL, heartbeat), full jitter
// ---------------------------------------------------------------------------

/// A budget large enough never to be the binding constraint in the cases below.
const AMPLE: Duration = Duration::from_secs(30);

#[test]
fn the_cap_is_the_heartbeat_when_the_lease_outlives_it() {
    assert_eq!(
        wake_cap(Some(Duration::from_secs(10)), AMPLE),
        HEARTBEAT,
        "a long lease must not stretch the wake past the safety-net heartbeat"
    );
}

#[test]
fn the_cap_is_the_lease_when_it_expires_first() {
    // The reason `PTTL` is read at all: a lock due in 40 ms should be retried
    // in 40 ms, not waited out for the heartbeat.
    let pttl = Duration::from_millis(40);
    assert_eq!(wake_cap(Some(pttl), AMPLE), pttl);
}

#[test]
fn an_unreadable_lease_falls_back_to_the_heartbeat() {
    assert_eq!(wake_cap(None, AMPLE), HEARTBEAT);
}

#[test]
fn the_cap_never_exceeds_the_callers_remaining_budget() {
    let remaining = Duration::from_millis(5);
    assert_eq!(
        wake_cap(Some(Duration::from_secs(10)), remaining),
        remaining,
        "a waiter with 5ms left must not sleep 250ms and report LockTimeout late"
    );
}

#[test]
fn the_delay_stays_inside_its_cap() {
    let pttl = Duration::from_millis(40);
    for _draw in 0..1_000 {
        let delay = wake_delay(Some(pttl), AMPLE);
        assert!(
            delay <= pttl,
            "full jitter must stay within min(PTTL, heartbeat), got {delay:?}"
        );
    }
}

#[test]
fn the_delay_is_bounded_by_the_heartbeat_however_long_the_lease() {
    for _draw in 0..1_000 {
        assert!(wake_delay(Some(Duration::from_hours(1)), AMPLE) <= HEARTBEAT);
    }
}

#[test]
fn the_delay_varies_across_draws() {
    // Asserted as non-identity rather than as a distribution: the property that
    // matters is that two instances contending for one name do not retry on the
    // same schedule (DESIGN.md §5.3). With a 250 ms cap at nanosecond
    // resolution, 64 identical draws is not a run this can see by chance.
    let draws: Vec<Duration> = (0..64).map(|_| wake_delay(None, AMPLE)).collect();
    let first = draws[0];
    assert!(
        draws.iter().any(|delay| *delay != first),
        "an unjittered retry schedule turns a hot lock into a synchronized \
         fleet-wide SET NX burst"
    );
}

#[test]
fn a_spent_budget_yields_no_sleep_rather_than_a_panic() {
    // The degenerate input the acquisition loop can genuinely produce: the
    // deadline check and the sleep are not one atomic step, so `remaining` can
    // reach zero in between. An exclusive `0..0` range would panic here.
    assert_eq!(
        wake_delay(Some(Duration::ZERO), Duration::ZERO),
        Duration::ZERO
    );
}
