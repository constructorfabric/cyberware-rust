// Updated: 2026-09-10 — tests for the `CredStorePluginClientV1` trait impl
// against the `tenant_id/value_id` SPI (ADR-0006).
use credstore_sdk::{CredStoreError, CredStorePluginClientV1, SecretValue, TenantId, ValueId};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::StaticCredStorePluginConfig;
use crate::domain::service::Service;

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_tenant_id(Uuid::new_v4())
        .subject_id(Uuid::new_v4())
        .build()
        .expect("test security context")
}

fn empty_service() -> Service {
    Service::from_config(&StaticCredStorePluginConfig::default()).expect("config builds")
}

fn tid() -> TenantId {
    TenantId(Uuid::new_v4())
}
fn vid() -> ValueId {
    ValueId::new_v4()
}

#[tokio::test]
async fn get_missing_returns_none() {
    let svc = empty_service();
    let got = svc.get(&ctx(), &tid(), &vid()).await.unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn put_then_get_roundtrip() {
    let svc = empty_service();
    let (t, v) = (tid(), vid());

    svc.put(&ctx(), &t, &v, SecretValue::from("v1"))
        .await
        .unwrap();

    let got = svc.get(&ctx(), &t, &v).await.unwrap();
    assert_eq!(got.unwrap().as_bytes(), b"v1");
}

#[tokio::test]
async fn put_on_existing_id_conflicts() {
    let svc = empty_service();
    let (t, v) = (tid(), vid());

    svc.put(&ctx(), &t, &v, SecretValue::from("v1"))
        .await
        .unwrap();
    let err = svc
        .put(&ctx(), &t, &v, SecretValue::from("v2"))
        .await
        .unwrap_err();
    assert!(matches!(err, CredStoreError::Conflict));
}

#[tokio::test]
async fn delete_removes_value() {
    let svc = empty_service();
    let (t, v) = (tid(), vid());

    svc.put(&ctx(), &t, &v, SecretValue::from("v1"))
        .await
        .unwrap();
    svc.delete(&ctx(), &t, &v).await.unwrap();
    assert!(svc.get(&ctx(), &t, &v).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_missing_is_success() {
    let svc = empty_service();
    svc.delete(&ctx(), &tid(), &vid()).await.unwrap();
}
