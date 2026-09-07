#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use super::PdpReachability;
use crate::domain::error::DomainError;
use crate::domain::ports::pdp::PdpProbe;
use crate::test_support::{DenyAllPdp, FailingPdp, HangingPdp, PermitTenantsPdp, tenant};

#[tokio::test]
async fn a_permitting_pdp_is_reachable() {
    let pdp = Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()]));
    PdpReachability::new(pdp.clone())
        .probe()
        .await
        .expect("the PDP answered");
    assert_eq!(pdp.calls(), 1, "exactly one round trip");
}

#[tokio::test]
async fn a_denying_pdp_is_reachable() {
    // The probe principal belongs to no tenant, so a real PDP denies it. The
    // denial is the expected answer: the probe measures reachability, not
    // permission.
    PdpReachability::new(Arc::new(DenyAllPdp))
        .probe()
        .await
        .expect("a denial is an answer");
}

#[tokio::test]
async fn a_failing_pdp_is_unavailable() {
    let err = PdpReachability::new(Arc::new(FailingPdp))
        .probe()
        .await
        .expect_err("a transport error is unavailability");
    assert!(matches!(err, DomainError::PdpUnavailable(_)), "{err:?}");
}

#[tokio::test]
async fn a_pdp_that_does_not_answer_within_the_deadline_is_unavailable() {
    let err = PdpReachability::new(Arc::new(HangingPdp))
        .with_deadline(Duration::from_millis(20))
        .probe()
        .await
        .expect_err("the deadline bounds the probe");
    match err {
        DomainError::PdpUnavailable(reason) => {
            assert!(reason.contains("did not answer"), "{reason}");
        }
        other => panic!("expected PdpUnavailable, got {other:?}"),
    }
}
