use super::{Readiness, ReadinessState};
use crate::domain::error::Dependency;

#[test]
fn readiness_starts_pending_and_records_the_last_transition() {
    let readiness = Readiness::new();
    assert_eq!(readiness.snapshot(), ReadinessState::Starting);
    assert!(!readiness.is_ready());

    readiness.mark_failed(Dependency::Cluster, "profile unbound");
    assert_eq!(
        readiness.snapshot(),
        ReadinessState::Failed {
            dependency: Dependency::Cluster,
            reason: "profile unbound".to_owned(),
        }
    );
    assert!(!readiness.is_ready());

    readiness.mark_ready();
    assert!(readiness.is_ready());
    assert_eq!(readiness.snapshot(), ReadinessState::Ready);
}
