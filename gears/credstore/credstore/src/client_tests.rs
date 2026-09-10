//! Unit tests for [`CredStoreLocalClient`] and the `DomainError` →
//! `CredStoreError` SDK-error conversion.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use credstore_sdk::{
    CredStoreClientV1, CredStoreError, Fallback, PatchField, PutPrecondition, SecretRef,
    SecretValue, SharingMode, Validator, WritePrecondition,
};
use uuid::Uuid;

use super::{CredStoreLocalClient, DomainError, Service};
use crate::domain::ports::metrics::NoopMetrics;
use crate::domain::secret::service::GcSettings;
use crate::domain::secret::test_support::{
    FakeDir, FakePlugin, FakePluginSelector, FakeSecretRepo, catalog_type_resolver, make_ctx,
    mock_enforcer,
};

fn generic_write(value: &str) -> credstore_sdk::CredentialWrite {
    credstore_sdk::CredentialWrite {
        secret_type: Some(credstore_sdk::SecretType::generic().into()),
        sharing: SharingMode::Tenant,
        fallback: Fallback::Inherit,
        expires_at: None,
        value: SecretValue::from(value),
    }
}

fn replace_write(value: &str) -> credstore_sdk::CredentialWrite {
    credstore_sdk::CredentialWrite {
        secret_type: None,
        sharing: SharingMode::Tenant,
        fallback: Fallback::Inherit,
        expires_at: None,
        value: SecretValue::from(value),
    }
}

fn build_client(repo: Arc<FakeSecretRepo>, dir: Arc<FakeDir>) -> CredStoreLocalClient {
    let selector = Arc::new(FakePluginSelector::new(FakePlugin::new()));
    let svc = Arc::new(Service::new(
        repo,
        dir,
        mock_enforcer(),
        selector,
        catalog_type_resolver(),
        Arc::new(NoopMetrics),
        GcSettings {
            pending_max_age_secs: 3600,
            batch_size: 256,
        },
    ));
    CredStoreLocalClient::new(svc)
}

#[test]
#[allow(
    clippy::cognitive_complexity,
    reason = "a flat enumeration of every DomainError variant's SDK-error mapping; splitting it \
              would only scatter the one-to-one correspondence this test is checking"
)]
fn domain_error_maps_to_sdk_error() {
    assert!(matches!(
        CredStoreError::from(DomainError::NotFound),
        CredStoreError::NotFound
    ));
    assert!(matches!(
        CredStoreError::from(DomainError::Conflict),
        CredStoreError::Conflict
    ));
    assert!(matches!(
        CredStoreError::from(DomainError::InvalidSecretRef {
            detail: "x".to_owned()
        }),
        CredStoreError::InvalidSecretRef { .. }
    ));
    assert!(matches!(
        CredStoreError::from(DomainError::UnsupportedTransition {
            detail: "x".to_owned()
        }),
        CredStoreError::UnsupportedTransition { .. }
    ));
    assert!(matches!(
        CredStoreError::from(DomainError::AccessDenied { cause: None }),
        CredStoreError::AccessDenied
    ));
    assert!(matches!(
        CredStoreError::from(DomainError::ServiceUnavailable {
            detail: "x".to_owned(),
            retry_after: Some(Duration::from_secs(1)),
            cause: None,
        }),
        CredStoreError::ServiceUnavailable { .. }
    ));
    assert!(matches!(
        CredStoreError::from(DomainError::internal("x")),
        CredStoreError::Internal(_)
    ));
    assert!(matches!(
        CredStoreError::from(DomainError::TypeViolation {
            field: "type",
            reason: "TYPE_IMMUTABLE",
            detail: "x".to_owned(),
        }),
        CredStoreError::TypeViolation { .. }
    ));
    assert!(matches!(
        CredStoreError::from(DomainError::InvalidRequest {
            field: "value",
            reason: "EMPTY_PATCH",
            detail: "x".to_owned(),
        }),
        CredStoreError::InvalidRequest { .. }
    ));
    // The typed SDK cannot omit the precondition, so the domain guard crossing
    // the in-process boundary is an invariant breach, not a client error.
    assert!(matches!(
        CredStoreError::from(DomainError::PreconditionRequired {
            detail: "x".to_owned()
        }),
        CredStoreError::Internal(_)
    ));
}

#[tokio::test]
async fn local_client_round_trips_through_service() {
    let tenant = Uuid::new_v4();
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    let k = SecretRef::new("client-key").expect("ref");

    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    let client = build_client(repo, dir);

    client
        .put(&ctx, &k, generic_write("v"), PutPrecondition::CreateOnly)
        .await
        .expect("create");
    // Rotation without a version in hand: the explicit LWW opt-in.
    client
        .put(&ctx, &k, replace_write("v2"), PutPrecondition::Exists)
        .await
        .expect("put");
    assert!(client.get(&ctx, &k).await.expect("get").is_some());
    assert!(
        client
            .get_secret(&ctx, &k)
            .await
            .expect("get_secret")
            .is_some()
    );
    client
        .delete(&ctx, &k, WritePrecondition::Exists)
        .await
        .expect("delete");
    assert!(client.get(&ctx, &k).await.expect("get").is_none());
}

#[tokio::test]
async fn create_only_is_create_only_put_never_creates() {
    let tenant = Uuid::new_v4();
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    let k = SecretRef::new("create-key").expect("ref");

    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    let client = build_client(repo, dir);

    // First create succeeds.
    client
        .put(&ctx, &k, generic_write("v1"), PutPrecondition::CreateOnly)
        .await
        .expect("first create");
    // Second create-only of the same sharing class → Conflict.
    let err = client
        .put(&ctx, &k, generic_write("v2"), PutPrecondition::CreateOnly)
        .await
        .expect_err("second create conflicts");
    assert!(matches!(err, CredStoreError::Conflict));
    // `put` under `Exists` replaces the existing credential (no conflict)…
    client
        .put(&ctx, &k, replace_write("v3"), PutPrecondition::Exists)
        .await
        .expect("put replaces");
    // …but never creates: on a missing reference the mandatory precondition
    // fails as a Conflict regardless of the variant.
    let missing = SecretRef::new("create-key-missing").expect("ref");
    let err = client
        .put(&ctx, &missing, replace_write("v"), PutPrecondition::Exists)
        .await
        .expect_err("put on missing reference conflicts");
    assert!(matches!(err, CredStoreError::Conflict), "got: {err:?}");
}

#[tokio::test]
async fn precondition_guards_in_process_write_and_delete() {
    // The in-process client can now carry an optimistic-concurrency
    // precondition (the ClientHub equivalent of a REST `If-Match`), not just
    // the REST surface.
    let tenant = Uuid::new_v4();
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    let k = SecretRef::new("guarded-key").expect("ref");

    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    let client = build_client(repo, dir);

    client
        .put(&ctx, &k, generic_write("v1"), PutPrecondition::CreateOnly)
        .await
        .expect("create");
    let observed = client.get(&ctx, &k).await.expect("get").expect("present");
    let observed_validator = observed.validator.expect("own row has a strong validator");
    let stale = WritePrecondition::Matches {
        id: observed_validator.id,
        version: observed_validator.version,
    };

    // A guarded patch against the observed generation succeeds and bumps the
    // version, so the same validator is now stale.
    client
        .patch(
            &ctx,
            &k,
            credstore_sdk::CredentialPatch {
                secret_type: None,
                sharing: None,
                fallback: None,
                expires_at: PatchField::Absent,
                value: PatchField::Set(SecretValue::from("v2")),
            },
            stale,
        )
        .await
        .expect("guarded patch matches current generation");

    // Re-using the stale validator is rejected as a conflict.
    let err = client
        .patch(
            &ctx,
            &k,
            credstore_sdk::CredentialPatch {
                secret_type: None,
                sharing: None,
                fallback: None,
                expires_at: PatchField::Absent,
                value: PatchField::Set(SecretValue::from("v3")),
            },
            stale,
        )
        .await
        .expect_err("stale precondition conflicts");
    assert!(matches!(err, CredStoreError::Conflict), "got: {err:?}");

    // A guarded delete: the stale validator conflicts, the current one succeeds.
    let current = client.get(&ctx, &k).await.expect("get").expect("present");
    let current_validator = current.validator.expect("own row has a strong validator");
    let err = client
        .delete(&ctx, &k, stale)
        .await
        .expect_err("stale delete conflicts");
    assert!(matches!(err, CredStoreError::Conflict), "got: {err:?}");
    client
        .delete(
            &ctx,
            &k,
            WritePrecondition::Matches {
                id: current_validator.id,
                version: current_validator.version,
            },
        )
        .await
        .expect("guarded delete matches current generation");
    assert!(client.get(&ctx, &k).await.expect("get").is_none());
}

#[tokio::test]
async fn patch_precondition_maps_sdk_matches_to_domain_version() {
    // Exercises `to_domain_precondition`'s `Matches` arm directly (the
    // `Exists` arm is already covered by the tests above).
    let tenant = Uuid::new_v4();
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    let k = SecretRef::new("k").expect("ref");
    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    let client = build_client(repo, dir);

    client
        .put(&ctx, &k, generic_write("v1"), PutPrecondition::CreateOnly)
        .await
        .expect("create");
    let cred = client.get(&ctx, &k).await.expect("get").expect("present");
    let v = cred.validator.expect("some");

    let out = client
        .put(
            &ctx,
            &k,
            replace_write("v2"),
            PutPrecondition::Matches(Validator {
                id: v.id,
                version: v.version,
            }),
        )
        .await
        .expect("matching put precondition");
    assert!(!out.created);
    assert_eq!(out.validator.version, v.version + 1);
}
