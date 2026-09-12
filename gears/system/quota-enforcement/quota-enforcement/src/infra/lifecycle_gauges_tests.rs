use super::LifecycleGaugeCell;
use crate::domain::ports::lifecycle_gauges::{LifecycleCounts, LifecycleGaugeSink};

#[test]
fn the_cell_starts_empty_and_holds_the_latest_publication() {
    let cell = LifecycleGaugeCell::default();
    assert_eq!(cell.load(), None);
    let first = LifecycleCounts {
        cap_zero: 1,
        cap_unbounded: 2,
        for_direct_metric: 3,
    };
    cell.publish(Some(first));
    assert_eq!(cell.load(), Some(first));
    let second = LifecycleCounts {
        cap_zero: 4,
        ..first
    };
    cell.publish(Some(second));
    assert_eq!(cell.load(), Some(second), "the latest sample wins");
    cell.publish(None);
    assert_eq!(cell.load(), None, "a withdrawal leaves nothing behind");
}
