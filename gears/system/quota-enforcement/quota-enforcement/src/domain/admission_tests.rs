use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use quota_enforcement_sdk::TenantId;
use toolkit_security::pep_properties;
use uuid::Uuid;

use super::{Admission, AdmissionTarget};
use crate::domain::error::DomainError;
use crate::domain::pep::{actions, resources};
use crate::domain::ports::metrics::DenialReason;
use crate::test_support::{
    DenyAllPdp, FailingPdp, PermitTenantsPdp, PermitUnconstrainedPdp, RecordingMetrics, ctx, tenant,
};

fn admission(
    pdp: Arc<dyn authz_resolver_sdk::AuthZResolverApi>,
) -> (Admission, Arc<RecordingMetrics>) {
    let metrics = Arc::new(RecordingMetrics::default());
    (
        Admission::new(PolicyEnforcer::new(pdp), metrics.clone()),
        metrics,
    )
}

#[tokio::test]
async fn a_permit_that_names_the_target_tenant_is_admitted_with_the_scope_unmodified() {
    let pdp = Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()]));
    let (admission, metrics) = admission(pdp.clone());

    let admitted = admission
        .admit(
            &ctx(),
            &resources::QUOTA,
            actions::CREATE,
            AdmissionTarget::tenant(tenant()),
        )
        .await
        .expect("admitted");
    assert_eq!(admitted.tenant_id, tenant());
    assert!(
        admitted
            .access_scope
            .contains_uuid(pep_properties::OWNER_TENANT_ID, tenant().as_uuid()),
        "the scope is passed through with its tenant constraint"
    );
    assert!(!admitted.access_scope.is_unconstrained());
    assert_eq!(pdp.calls(), 1, "exactly one PDP round trip");
    assert!(
        metrics.denials().is_empty(),
        "no denial recorded on admission"
    );
}

#[tokio::test]
async fn a_permit_for_other_tenants_is_denied_by_the_post_permit_gate() {
    let other = Uuid::from_u128(0xbeef);
    let (admission, metrics) = admission(Arc::new(PermitTenantsPdp::new(vec![other])));
    let err = admission
        .admit(
            &ctx(),
            &resources::QUOTA,
            actions::CREATE,
            AdmissionTarget::tenant(tenant()),
        )
        .await
        .expect_err("cross-tenant permit is a denial");
    assert_eq!(
        err,
        DomainError::PdpDenied {
            reason: Some(DomainError::TENANT_OUT_OF_SCOPE.to_owned()),
        }
    );
    assert_eq!(metrics.denials(), vec![DenialReason::PermissionDenied]);
}

#[tokio::test]
async fn an_unconstrained_permit_under_required_constraints_fails_closed() {
    let (admission, metrics) = admission(Arc::new(PermitUnconstrainedPdp));
    let err = admission
        .admit(
            &ctx(),
            &resources::OPERATION,
            actions::DEBIT,
            AdmissionTarget::tenant(tenant()),
        )
        .await
        .expect_err("missing constraints never widen access");
    assert!(matches!(err, DomainError::PdpDenied { .. }), "{err:?}");
    assert_eq!(metrics.denials(), vec![DenialReason::PermissionDenied]);
}

#[tokio::test]
async fn an_explicit_denial_is_permission_denied() {
    let (admission, metrics) = admission(Arc::new(DenyAllPdp));
    let err = admission
        .admit(
            &ctx(),
            &resources::LEASE,
            actions::RESERVE,
            AdmissionTarget::tenant(tenant()),
        )
        .await
        .expect_err("denied");
    assert!(matches!(err, DomainError::PdpDenied { .. }), "{err:?}");
    assert_eq!(metrics.denials(), vec![DenialReason::PermissionDenied]);
}

#[tokio::test]
async fn an_unreachable_pdp_is_unavailable_never_a_permit() {
    let (admission, metrics) = admission(Arc::new(FailingPdp));
    let err = admission
        .admit(
            &ctx(),
            &resources::QUOTA,
            actions::GET,
            AdmissionTarget::tenant(tenant()),
        )
        .await
        .expect_err("fail closed");
    assert!(matches!(err, DomainError::PdpUnavailable(_)), "{err:?}");
    assert_eq!(metrics.denials(), vec![DenialReason::PdpUnavailable]);
}

#[tokio::test]
async fn malformed_targets_are_rejected_before_any_pdp_call() {
    let pdp = Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()]));
    let (admission, metrics) = admission(pdp.clone());

    let nil_tenant = admission
        .admit(
            &ctx(),
            &resources::QUOTA,
            actions::CREATE,
            AdmissionTarget::tenant(TenantId::new(Uuid::nil())),
        )
        .await
        .expect_err("nil tenant");
    assert_eq!(
        nil_tenant,
        DomainError::InvalidArgument {
            field: "tenant_id",
            reason: "TENANT_ID_REQUIRED",
        }
    );
    let nil_resource = admission
        .admit(
            &ctx(),
            &resources::QUOTA,
            actions::GET,
            AdmissionTarget::resource(tenant(), Uuid::nil()),
        )
        .await
        .expect_err("nil resource");
    assert_eq!(
        nil_resource,
        DomainError::InvalidArgument {
            field: "resource_id",
            reason: "RESOURCE_ID_INVALID",
        }
    );
    assert_eq!(pdp.calls(), 0, "shape checks run before the PDP");
    assert_eq!(
        metrics.denials(),
        vec![DenialReason::InvalidArgument, DenialReason::InvalidArgument]
    );
}

#[tokio::test]
async fn a_resource_target_forwards_the_resource_id_to_the_pdp() {
    let pdp = Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()]));
    let (admission, _) = admission(pdp.clone());
    let resource_id = Uuid::from_u128(0x77);
    admission
        .admit(
            &ctx(),
            &resources::QUOTA,
            actions::UPDATE,
            AdmissionTarget::resource(tenant(), resource_id),
        )
        .await
        .expect("admitted");
    assert_eq!(pdp.last_resource_id(), Some(resource_id.to_string()));
}
