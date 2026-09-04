use std::sync::Arc;

use quota_enforcement_sdk::testing::InMemoryStorage;
use toolkit::ClientHub;
use toolkit_canonical_errors::CanonicalError;

use super::PluginBinding;
use crate::domain::error::{DomainError, PluginKind};
use crate::test_support::{PluginFixture, hub_with, register_storage, storage_instance};

fn binding(hub: Arc<ClientHub>) -> PluginBinding {
    PluginBinding::new(hub, "acme".to_owned())
}

#[tokio::test]
async fn resolves_the_lowest_priority_instance_of_the_configured_vendor() {
    let storage_hi = storage_instance("cf.core._.qe_hi.v1", "acme", 50);
    let storage_lo = storage_instance("cf.core._.qe_lo.v1", "acme", 10);
    let other_vendor = storage_instance("cf.core._.qe_other.v1", "globex", 1);
    let hub = hub_with(&[&storage_hi, &storage_lo, &other_vendor]);
    let lo = Arc::new(InMemoryStorage::new());
    register_storage(&hub, &storage_hi, Arc::new(InMemoryStorage::new()));
    register_storage(&hub, &storage_lo, lo.clone());

    let binding = binding(hub);
    let storage = binding.resolve_storage().await.expect("storage resolved");
    storage
        .bootstrap(&quota_enforcement_sdk::BootstrapBundle::foundation())
        .await
        .expect("the resolved handle is the lowest-priority instance");
    assert_eq!(
        lo.bootstrap_calls(),
        1,
        "the priority-10 instance was chosen"
    );
}

#[tokio::test]
async fn a_vendor_without_instances_is_plugin_not_found() {
    let other_vendor = storage_instance("cf.core._.qe_other.v1", "globex", 1);
    let hub = hub_with(&[&other_vendor]);
    let err = binding(hub)
        .resolve_storage()
        .await
        .err()
        .expect("no acme instance");
    assert_eq!(
        err,
        DomainError::PluginNotFound {
            kind: PluginKind::Storage,
            vendor: "acme".to_owned(),
        }
    );
}

#[tokio::test]
async fn an_instance_without_a_scoped_client_is_reported_as_unregistered() {
    let storage = storage_instance("cf.core._.qe_db_storage.v1", "acme", 100);
    let hub = hub_with(&[&storage]);
    let err = binding(hub)
        .resolve_storage()
        .await
        .err()
        .expect("client missing");
    assert_eq!(
        err,
        DomainError::PluginClientNotRegistered {
            kind: PluginKind::Storage,
            gts_id: storage.instance_id.clone(),
        }
    );
}

#[tokio::test]
async fn a_malformed_instance_is_reported_with_its_id() {
    let broken = PluginFixture::malformed_storage("cf.core._.qe_broken.v1");
    let hub = hub_with(&[&broken]);
    let err = binding(hub)
        .resolve_storage()
        .await
        .err()
        .expect("malformed");
    assert!(
        matches!(
            &err,
            DomainError::InvalidPluginInstance { kind: PluginKind::Storage, gts_id, .. }
                if gts_id == &broken.instance_id
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn registry_failures_and_a_missing_registry_are_unavailability() {
    let no_registry = Arc::new(ClientHub::new());
    let err = binding(no_registry)
        .resolve_storage()
        .await
        .err()
        .expect("no registry");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "{err:?}"
    );

    let hub = crate::test_support::hub_with_failing_registry(
        CanonicalError::service_unavailable()
            .with_detail("registry down")
            .create(),
    );
    let err = binding(hub)
        .resolve_storage()
        .await
        .err()
        .expect("registry error");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "{err:?}"
    );
}
