//! Pure: `now` is passed in, so nothing here waits.

use std::time::Duration;

use tokio::time::Instant;

use super::heartbeat::HeartbeatSchedule;

const INTERVAL: Duration = Duration::from_secs(5);

#[test]
fn a_fresh_schedule_is_not_immediately_due() {
    let now = Instant::now();
    let schedule = HeartbeatSchedule::new(INTERVAL, now);

    assert!(!schedule.is_due(now));
    assert_eq!(schedule.next_due(), now + INTERVAL);
}

#[test]
fn it_falls_due_exactly_one_interval_after_the_last_frame() {
    let now = Instant::now();
    let schedule = HeartbeatSchedule::new(INTERVAL, now);

    assert!(!schedule.is_due(now + INTERVAL - Duration::from_millis(1)));
    assert!(schedule.is_due(now + INTERVAL));
    assert!(schedule.is_due(now + Duration::from_mins(1)));
}

#[test]
fn any_frame_resets_the_timer_not_only_an_event_frame() {
    let now = Instant::now();
    let mut schedule = HeartbeatSchedule::new(INTERVAL, now);

    // A topology or progress frame has already told the consumer the stream is
    // alive, which is the only thing a heartbeat says - so sending one right
    // after is noise the consumer parses and discards.
    schedule.record_frame(now + Duration::from_secs(4));

    assert!(!schedule.is_due(now + Duration::from_secs(5)));
    assert!(schedule.is_due(now + Duration::from_secs(9)));
}

#[test]
fn recording_repeatedly_keeps_pushing_the_deadline_out() {
    let now = Instant::now();
    let mut schedule = HeartbeatSchedule::new(INTERVAL, now);

    for tick in 1..=10 {
        let at = now + Duration::from_secs(tick);
        schedule.record_frame(at);
        assert!(
            !schedule.is_due(at),
            "a stream delivering steadily never owes a heartbeat"
        );
    }
    assert_eq!(schedule.next_due(), now + Duration::from_secs(15));
}

#[test]
fn the_interval_is_reported_as_configured() {
    let schedule = HeartbeatSchedule::new(INTERVAL, Instant::now());

    assert_eq!(schedule.interval(), INTERVAL);
}
