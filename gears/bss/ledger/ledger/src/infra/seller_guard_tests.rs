use std::sync::Arc;

use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::*;

const SELLER: &str = gts_id!("cf.core.am.tenant_type.v1~cf.core.am.partner.v1~");
const BUYER: &str = gts_id!("cf.core.am.tenant_type.v1~cf.core.am.organization.v1~");
/// Family wildcard covering every registered tenant type.
const ANY_TYPE: &str = gts_id!("cf.core.am.tenant_type.v1~*");

/// Canned tenant-type reader (stands in for the AM `get_tenant` adapter).
struct FakeReader(Option<String>);

#[async_trait]
impl TenantTypeReader for FakeReader {
    async fn tenant_type(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
    ) -> Result<Option<String>, CanonicalError> {
        Ok(self.0.clone())
    }
}

/// A guard whose configured seller set is exactly `{SELLER}`.
fn guard(tenant_type: Option<&str>) -> SellerGuard {
    SellerGuard::new(
        Arc::new(FakeReader(tenant_type.map(str::to_owned))),
        [SELLER.to_owned()],
    )
}

#[tokio::test]
async fn seller_type_passes() {
    let ctx = SecurityContext::anonymous();
    assert!(
        guard(Some(SELLER))
            .assert_owns_ledger(&ctx, Uuid::now_v7())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn non_seller_type_is_rejected() {
    let ctx = SecurityContext::anonymous();
    let err = guard(Some(BUYER))
        .assert_owns_ledger(&ctx, Uuid::now_v7())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CanonicalError::FailedPrecondition { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn unresolved_type_is_rejected() {
    let ctx = SecurityContext::anonymous();
    let err = guard(None)
        .assert_owns_ledger(&ctx, Uuid::now_v7())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CanonicalError::FailedPrecondition { .. }),
        "got {err:?}"
    );
}

/// A reader that surfaces a transient AM error (models `get_tenant` failing).
struct FailingReader;

#[async_trait]
impl TenantTypeReader for FailingReader {
    async fn tenant_type(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
    ) -> Result<Option<String>, CanonicalError> {
        Err(CanonicalError::service_unavailable().create())
    }
}

/// A non-precondition AM error (transient / `NotFound`) must propagate
/// UNCHANGED, not be collapsed into the `FailedPrecondition` seller-reject —
/// otherwise a registry blip would masquerade as "not a ledger owner".
#[tokio::test]
async fn am_error_propagates_unchanged() {
    let ctx = SecurityContext::anonymous();
    let g = SellerGuard::new(Arc::new(FailingReader), [SELLER.to_owned()]);
    let err = g
        .assert_owns_ledger(&ctx, Uuid::now_v7())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CanonicalError::ServiceUnavailable { .. }),
        "transient AM error must propagate, not collapse to FailedPrecondition; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Family-wildcard entries (`prefix*`)
// ---------------------------------------------------------------------------

/// A guard configured with `seller_types`, reading back `tenant_type`.
fn guard_with(seller_types: &[&str], tenant_type: Option<&str>) -> SellerGuard {
    SellerGuard::new(
        Arc::new(FakeReader(tenant_type.map(str::to_owned))),
        seller_types.iter().map(|s| (*s).to_owned()),
    )
}

#[tokio::test]
async fn family_wildcard_admits_every_tenant_type() {
    let ctx = SecurityContext::anonymous();
    for tenant_type in [SELLER, BUYER] {
        assert!(
            guard_with(&[ANY_TYPE], Some(tenant_type))
                .assert_owns_ledger(&ctx, Uuid::now_v7())
                .await
                .is_ok(),
            "`…tenant_type.v1~*` must admit {tenant_type}"
        );
    }
}

#[tokio::test]
async fn family_wildcard_does_not_admit_another_envelope() {
    let ctx = SecurityContext::anonymous();
    let other_envelope = gts_id!("cf.core.am.tenant.v1~cf.core.am.partner.v1~");
    assert!(
        guard_with(&[ANY_TYPE], Some(other_envelope))
            .assert_owns_ledger(&ctx, Uuid::now_v7())
            .await
            .is_err(),
        "the wildcard is anchored on the tenant_type envelope, not on `gts.`"
    );
}

/// The separator before `*` is retained when compiling, so a wildcard cannot
/// bleed into a longer sibling segment.
#[tokio::test]
async fn family_wildcard_keeps_its_separator() {
    let ctx = SecurityContext::anonymous();
    let partner_family = gts_id!("cf.core.am.tenant_type.v1~cf.core.am.partner.*");
    let lookalike = gts_id!("cf.core.am.tenant_type.v1~cf.core.am.partnership.v1~");
    assert!(
        guard_with(&[partner_family], Some(lookalike))
            .assert_owns_ledger(&ctx, Uuid::now_v7())
            .await
            .is_err(),
        "`partner.*` must not match `partnership…`"
    );
}

#[tokio::test]
async fn exact_and_wildcard_entries_compose() {
    let ctx = SecurityContext::anonymous();
    let recognized_family = gts_id!("cf.core.am.tenant_type.v1~cf.core.am.platform.*");
    let platform = gts_id!("cf.core.am.tenant_type.v1~cf.core.am.platform.v1~");
    // Exact entry still admits its own type...
    assert!(
        guard_with(&[SELLER, recognized_family], Some(SELLER))
            .assert_owns_ledger(&ctx, Uuid::now_v7())
            .await
            .is_ok()
    );
    // ...and the wildcard entry admits the family beside it.
    assert!(
        guard_with(&[SELLER, recognized_family], Some(platform))
            .assert_owns_ledger(&ctx, Uuid::now_v7())
            .await
            .is_ok()
    );
    // Anything in neither is still rejected.
    assert!(
        guard_with(&[SELLER, recognized_family], Some(BUYER))
            .assert_owns_ledger(&ctx, Uuid::now_v7())
            .await
            .is_err()
    );
}
