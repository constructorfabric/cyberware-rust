//! Integration test for `ScopeValidator::validate_scope_exists` — RG
//! tenant-ownership mismatch driven through the full in-process pipeline
//! (no Docker required).

mod common;

use std::sync::Arc;

use common::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};
use rbac::domain::rg_port::RbacRgRead;
use rbac::domain::scope_validator::{MissingScopeEntity, ScopeError, ScopeValidator};
use tenant_resolver_sdk::TenantResolverClient;
use uuid::Uuid;

/// RG owned by T2 but claimed under T1 → `ScopeError::ScopeNotFound`
/// with `MissingScopeEntity::ResourceGroupOwnerMismatch`.
#[tokio::test]
async fn i28_rg_tenant_ownership_mismatch() {
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let rg1 = Uuid::new_v4();

    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])) as Arc<dyn TenantResolverClient>,
        Arc::new(FakeRbacRgRead::default().with_group(rg1, t2)) as Arc<dyn RbacRgRead>,
    );

    let scope = format!("/tenants/{t1}/resourceGroups/{rg1}");
    let ctx = toolkit_security::SecurityContext::anonymous();

    match validator.validate_scope_exists(&ctx, &scope).await {
        Err(ScopeError::ScopeNotFound {
            scope: returned_scope,
            missing:
                MissingScopeEntity::ResourceGroupOwnerMismatch {
                    rg_id,
                    claimed_tenant_id,
                    actual_tenant_id,
                },
        }) => {
            assert_eq!(
                returned_scope, scope,
                "scope string must be the original input"
            );
            assert_eq!(rg_id, rg1, "rg_id must be the requested group");
            assert_eq!(
                claimed_tenant_id, t1,
                "claimed_tenant_id must be T1 from the path"
            );
            assert_eq!(
                actual_tenant_id, t2,
                "actual_tenant_id must be T2 (the real owner)"
            );
        }
        other => unreachable!("expected ResourceGroupOwnerMismatch, got {other:?}"),
    }
}
