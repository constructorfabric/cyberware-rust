#![allow(clippy::expect_used)]

use std::sync::Arc;

use quota_enforcement_sdk::{
    PageRequest, QuotaFilter, QuotaId, QuotaPatch, StorageError, SubjectRef,
};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::StoragePlugin;
use crate::domain::ports::{Actor, StoreError};
use crate::infra::storage::SqlFoundationStore;
use crate::test_support::{FakeQuotaStore, draft, tenant, test_db};

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x5eed))
        .subject_type("service")
        .subject_tenant_id(tenant().as_uuid())
        .build()
        .expect("security context")
}

async fn plugin_over(store: FakeQuotaStore) -> (StoragePlugin, Arc<FakeQuotaStore>) {
    let store = Arc::new(store);
    let db = test_db().await;
    let plugin = StoragePlugin::new(Arc::new(SqlFoundationStore::new(db)), store.clone());
    (plugin, store)
}

#[tokio::test]
async fn every_store_error_lifts_to_its_contract_error() {
    let id = QuotaId::generate();
    let cases: Vec<(StoreError, StorageError)> = vec![
        (
            StoreError::Unavailable {
                operation: "create quota",
            },
            StorageError::Unavailable("database call failed during create quota".to_owned()),
        ),
        (
            StoreError::QuotaNotFound { id },
            StorageError::QuotaNotFound { id },
        ),
        (
            StoreError::QuotaDeactivated { id },
            StorageError::QuotaDeactivated { id },
        ),
        (
            StoreError::CapBelowConsumed {
                new_cap: 3,
                consumed: 5,
            },
            StorageError::CapBelowConsumed {
                new_cap: 3,
                consumed: 5,
            },
        ),
        (
            StoreError::ThresholdsRequireBoundedCap,
            StorageError::ThresholdsRequireBoundedCap,
        ),
        (
            StoreError::SubjectOutOfScope,
            StorageError::SubjectOutOfScope,
        ),
        (StoreError::InvalidCursor, StorageError::InvalidCursor),
        (
            StoreError::InvalidPatch {
                detail: "metadata patch carries no constraint contract".to_owned(),
            },
            StorageError::Internal(
                "invalid patch: metadata patch carries no constraint contract".to_owned(),
            ),
        ),
        (
            StoreError::InvalidFilter {
                detail: "too many ids".to_owned(),
            },
            StorageError::Internal("invalid filter: too many ids".to_owned()),
        ),
        (
            StoreError::ValueOutOfRange {
                field: "cap",
                value: "18446744073709551615".to_owned(),
            },
            StorageError::Internal(
                "cap=18446744073709551615 does not fit the column type".to_owned(),
            ),
        ),
        (
            StoreError::Corrupt {
                operation: "update quota",
                detail: "moved".to_owned(),
            },
            StorageError::Internal(
                "storage state is inconsistent during update quota: moved".to_owned(),
            ),
        ),
    ];
    for (store_err, expected) in cases {
        let (plugin, _) = plugin_over(FakeQuotaStore::failing(store_err.clone())).await;
        let err = plugin
            .update_quota(
                &ctx(),
                &AccessScope::allow_all(),
                id,
                QuotaPatch::default(),
                &[],
            )
            .await
            .expect_err("store fails");
        assert_eq!(err, expected, "{store_err:?}");
    }
}

#[tokio::test]
async fn the_security_context_becomes_the_operation_log_actor() {
    let (plugin, store) = plugin_over(FakeQuotaStore::default()).await;
    let scope = AccessScope::for_tenant(tenant().as_uuid());
    plugin
        .create_quota(&ctx(), &scope, draft(tenant(), "u1", Some(1)), &[])
        .await
        .expect("created");
    plugin
        .deactivate_quota(&ctx(), &scope, QuotaId::generate(), &[])
        .await
        .expect("deactivated");
    assert_eq!(
        store.actors(),
        vec![
            Actor {
                subject_id: Uuid::from_u128(0x5eed),
                subject_type: Some("service".to_owned()),
            };
            2
        ]
    );
}

#[tokio::test]
async fn reads_forward_with_the_caller_scope_and_the_contract_signatures() {
    let (plugin, _) = plugin_over(FakeQuotaStore::default()).await;
    let page = plugin
        .read_quotas(
            &ctx(),
            &AccessScope::allow_all(),
            QuotaFilter {
                subject: Some(SubjectRef {
                    projection_type: gts::GtsTypeId::new("gts.cf.core.qe.subj.v1~x.y.v1"),
                    subject_id: "s".to_owned(),
                }),
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("page");
    assert!(page.items.is_empty());
    assert!(
        plugin
            .read_active_projection_bindings()
            .await
            .expect("bindings")
            .is_empty()
    );
    assert_eq!(
        plugin
            .read_active_quota_counts()
            .await
            .expect("counts")
            .cap_zero,
        0
    );
    let store: &dyn crate::domain::ports::QuotaStore = plugin.quota_store();
    assert!(store.read_active_quota_counts().await.is_ok());
}
