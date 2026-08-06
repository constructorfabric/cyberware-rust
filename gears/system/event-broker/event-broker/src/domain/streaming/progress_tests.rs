//! Pure: no read set, no reader, no clock read.

use std::time::Duration;

use tokio::time::Instant;

use super::progress::{ProgressConfig, ProgressPolicy};

fn config() -> ProgressConfig {
    ProgressConfig {
        drift_threshold: 1000,
        min_interval: Duration::from_secs(5),
    }
}

#[test]
fn a_fresh_policy_is_not_immediately_due() {
    let now = Instant::now();
    let policy = ProgressPolicy::new(&config(), now);

    assert!(!policy.due(now));
    assert_eq!(policy.next_due(), now + Duration::from_secs(5));
}

#[test]
fn it_falls_due_once_the_rate_floor_has_elapsed() {
    let now = Instant::now();
    let policy = ProgressPolicy::new(&config(), now);

    assert!(!policy.due(now + Duration::from_secs(5) - Duration::from_millis(1)));
    assert!(policy.due(now + Duration::from_secs(5)));
}

#[test]
fn emitting_restarts_the_floor() {
    let now = Instant::now();
    let mut policy = ProgressPolicy::new(&config(), now);
    assert!(policy.due(now + Duration::from_secs(5)));

    policy.record_emitted(now + Duration::from_secs(5));

    // Without the floor a heavily filtered stream would emit one frame per
    // batch, each carrying almost no new information.
    assert!(!policy.due(now + Duration::from_secs(9)));
    assert!(policy.due(now + Duration::from_secs(10)));
}

#[test]
fn it_reports_the_drift_threshold_and_holds_no_position() {
    let policy = ProgressPolicy::new(&config(), Instant::now());

    // The session asks the read set which partitions drifted this far. This type
    // deliberately knows nothing about positions - an earlier design had it take
    // the read set, which pulled a reader stub into every scheduling test.
    assert_eq!(policy.drift_threshold(), 1000);
}

#[test]
fn being_due_says_nothing_about_anything_having_drifted() {
    let now = Instant::now();
    let policy = ProgressPolicy::new(&config(), now);

    // `due` is a rate question only. A stream that is due and has drifted
    // nowhere emits nothing, and that decision belongs to the session.
    assert!(policy.due(now + Duration::from_secs(5)));
}

#[test]
fn a_zero_floor_is_due_at_once() {
    let now = Instant::now();
    let policy = ProgressPolicy::new(
        &ProgressConfig {
            drift_threshold: 1,
            min_interval: Duration::ZERO,
        },
        now,
    );

    // Documented consequence of turning the floor off: every drifted batch
    // reports. Useful in a test, wasteful in production.
    assert!(policy.due(now));
}
