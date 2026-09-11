//! Pure: `now` is passed in, so nothing here waits on a clock.

use std::time::Duration;

use tokio::time::Instant;

use super::poll::{PollPolicy, TailPoll};

fn policy() -> PollPolicy {
    PollPolicy::from_floor(Duration::from_millis(1)).up_to(Duration::from_millis(8))
}

#[test]
fn a_fresh_poll_is_ready_at_once() {
    let poll = TailPoll::new(policy());

    assert!(poll.is_ready(Instant::now()));
}

#[test]
fn an_empty_fetch_defers_the_next_one() {
    let now = Instant::now();
    let mut poll = TailPoll::new(policy());

    poll.found_nothing(now, policy());

    assert!(!poll.is_ready(now), "asking again immediately would spin");
    assert!(poll.is_ready(now + Duration::from_millis(1)));
}

#[test]
fn repeated_empty_fetches_back_off_geometrically() {
    let now = Instant::now();
    let mut poll = TailPoll::new(policy());

    poll.found_nothing(now, policy());
    assert_eq!(poll.backoff(), Duration::from_millis(2));
    poll.found_nothing(now, policy());
    assert_eq!(poll.backoff(), Duration::from_millis(4));
    poll.found_nothing(now, policy());
    assert_eq!(poll.backoff(), Duration::from_millis(8));
}

#[test]
fn the_backoff_stops_at_the_ceiling() {
    let now = Instant::now();
    let mut poll = TailPoll::new(policy());

    for _ in 0..20 {
        poll.found_nothing(now, policy());
    }

    assert_eq!(
        poll.backoff(),
        Duration::from_millis(8),
        "an idle partition must cost almost nothing, not nothing at all"
    );
}

#[test]
fn finding_events_resets_the_backoff_and_readies_the_next_fetch() {
    let now = Instant::now();
    let mut poll = TailPoll::new(policy());
    for _ in 0..5 {
        poll.found_nothing(now, policy());
    }
    assert!(!poll.is_ready(now));

    poll.found_events(policy());

    // The tail is real and moving, so the next ask should not be delayed at all.
    assert!(poll.is_ready(now));
    assert_eq!(poll.backoff(), Duration::from_millis(1));
}
