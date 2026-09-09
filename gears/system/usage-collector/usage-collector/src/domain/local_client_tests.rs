//! Unit tests for the local client surface.
//!
//! Coverage: the service-delegating methods (catalog admin + usage-record
//! ingestion + deactivation + read-by-id) clear the trait boundary and hit
//! the PDP preflight inside the domain service — with an unreachable PDP
//! and/or a missing storage plugin the surface error is a fail-closed
//! envelope.

use std::sync::Arc;

use toolkit::client_hub::ClientHub;
use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;
use usage_collector_sdk::{
    UsageCollectorClientV1, UsageCollectorError, UsageKind, UsageType, UsageTypeGtsId,
};
use uuid::Uuid;

use crate::domain::test_support::{UnreachableResolver, enforcer_for};

use super::*;

const SAMPLE_USAGE_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

fn make_client() -> UsageCollectorLocalClient {
    let hub = Arc::new(ClientHub::new());
    let enforcer = enforcer_for(Arc::new(UnreachableResolver));
    let svc = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));
    UsageCollectorLocalClient::new(svc)
}

fn authenticated_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .build()
        .expect("authenticated context")
}

fn sample_gts_id() -> UsageTypeGtsId {
    UsageTypeGtsId::new(SAMPLE_USAGE_TYPE_ID).expect("valid usage_record-derived usage-type gts_id")
}

fn sample_register_input() -> UsageType {
    UsageType {
        gts_id: sample_gts_id(),
        kind: UsageKind::Counter,
        metadata_fields: ["tenant_id", "subject_id"]
            .into_iter()
            .map(|k| usage_collector_sdk::MetadataKey::new(k).expect("valid metadata key fixture"))
            .collect(),
    }
}

#[tokio::test]
async fn deactivate_usage_record_fails_closed_without_plugin_or_pdp() {
    // `deactivate_usage_record` now resolves the storage plugin first
    // (so it can pre-fetch the target record and feed the loaded
    // attribution tuple into PDP). With NO plugin registered in the hub
    // AND an unreachable PDP, the call MUST still fail closed — the
    // observable error here is `PluginUnavailable` (plugin resolution
    // runs before authz now), which lifts to `ServiceUnavailable` at the
    // canonical envelope boundary. The point of this smoke test is "the
    // SDK trait never silently succeeds when the host is misconfigured."
    let client = make_client();
    let err = client
        .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0xFEED))
        .await
        .expect_err("misconfigured host must fail closed");
    // Any of these variants is a fail-closed envelope — the specific one
    // depends on what the host resolves first (types-registry → plugin
    // selection → PDP). The invariant the smoke test guards is "never
    // Ok(()) when the host is misconfigured."
    assert!(
        matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
        "expected fail-closed envelope, got {err:?}"
    );
}

#[tokio::test]
async fn get_usage_record_fails_closed_without_plugin_or_pdp() {
    // `get_usage_record` resolves the storage plugin first so it can
    // pre-fetch the target record's attribution tuple before PDP
    // authorization. With NO plugin registered AND an unreachable PDP
    // the call MUST still fail closed — the observable error here is
    // `PluginUnavailable` (plugin resolution runs before authz), which
    // lifts to `ServiceUnavailable` at the canonical envelope boundary.
    // The smoke test guards "the SDK trait never silently succeeds when
    // the host is misconfigured."
    let client = make_client();
    let err = client
        .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0xFEED))
        .await
        .expect_err("misconfigured host must fail closed");
    assert!(
        matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
        "expected fail-closed envelope, got {err:?}"
    );
}

#[tokio::test]
async fn catalog_method_fails_on_authz_unavailable() {
    // The foundation catalog methods (register / read / list / delete) go
    // through the PDP preflight inside the domain service. With an unreachable
    // PDP transport, the preflight surfaces a deterministic ServiceUnavailable
    // (the AuthorizationUnavailable domain envelope lifts to ServiceUnavailable
    // on the SDK boundary). Every catalog method MUST share this envelope —
    // a regression that skipped the PDP preflight on one verb would show up
    // here as a non-ServiceUnavailable outcome.
    let client = make_client();

    let err = client
        .create_usage_type(&authenticated_ctx(), sample_register_input())
        .await
        .expect_err("create_usage_type with unreachable PDP must fail closed");
    assert!(
        matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
        "create_usage_type: expected ServiceUnavailable, got {err:?}"
    );

    let err = client
        .get_usage_type(&authenticated_ctx(), sample_gts_id())
        .await
        .expect_err("get_usage_type with unreachable PDP must fail closed");
    assert!(
        matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
        "get_usage_type: expected ServiceUnavailable, got {err:?}"
    );

    let err = client
        .list_usage_types(&authenticated_ctx(), &toolkit_odata::ODataQuery::default())
        .await
        .expect_err("list_usage_types with unreachable PDP must fail closed");
    assert!(
        matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
        "list_usage_types: expected ServiceUnavailable, got {err:?}"
    );

    let err = client
        .delete_usage_type(&authenticated_ctx(), sample_gts_id())
        .await
        .expect_err("delete_usage_type with unreachable PDP must fail closed");
    assert!(
        matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
        "delete_usage_type: expected ServiceUnavailable, got {err:?}"
    );
}
