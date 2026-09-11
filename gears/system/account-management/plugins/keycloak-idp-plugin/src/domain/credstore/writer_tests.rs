use super::*;
use crate::domain::system_actor::build_system_ctx;
use async_trait::async_trait;
use credstore_sdk::{
    Credential, CredentialListItem, CredentialPatch, PutOutcome, Secret, Validator,
};
use parking_lot::Mutex;
use toolkit_odata::{ODataQuery, Page};
use uuid::Uuid;

/// One recorded `put` invocation — `(key_str, value_bytes, sharing)`.
type PutCall = (String, Vec<u8>, SharingMode);

/// Stub unified client capturing `put` calls and serving one canned
/// `delete` response.
#[domain_model]
#[derive(Default)]
struct StubMutator {
    /// All recorded `put` invocations.
    put_calls: Mutex<Vec<PutCall>>,
    /// One-shot response for the next `delete` call; defaults to `Ok(())`.
    delete_response: Mutex<Option<Result<(), CredStoreError>>>,
}

#[async_trait]
impl CredStoreClientV1 for StubMutator {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
    ) -> Result<Option<Credential>, CredStoreError> {
        Ok(None)
    }

    async fn get_secret(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
    ) -> Result<Option<Secret>, CredStoreError> {
        Ok(None)
    }

    async fn put(
        &self,
        _ctx: &SecurityContext,
        key: &SecretRef,
        write: CredentialWrite,
        _precondition: PutPrecondition,
    ) -> Result<PutOutcome, CredStoreError> {
        self.put_calls.lock().push((
            key.as_ref().to_owned(),
            write.value.as_bytes().to_vec(),
            write.sharing,
        ));
        Ok(PutOutcome {
            created: true,
            validator: Validator {
                id: Uuid::nil(),
                version: 1,
            },
        })
    }

    async fn patch(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
        _patch: CredentialPatch,
        _precondition: WritePrecondition,
    ) -> Result<Validator, CredStoreError> {
        Ok(Validator {
            id: Uuid::nil(),
            version: 1,
        })
    }

    async fn delete(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
        _precondition: WritePrecondition,
    ) -> Result<(), CredStoreError> {
        self.delete_response.lock().take().unwrap_or(Ok(()))
    }

    async fn list(
        &self,
        _ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<CredentialListItem>, CredStoreError> {
        Ok(Page::empty(query.limit.unwrap_or(0)))
    }
}

#[tokio::test]
async fn put_forwards_with_system_ctx() {
    let stub = Arc::new(StubMutator::default());
    let ctx = build_system_ctx(Uuid::nil());
    let writer = CredStoreWriter::new(stub.clone(), ctx);
    let key = SecretRef::new("k").expect("valid SecretRef");
    let val = SecretValue::from("v");
    writer
        .put(&key, val, SharingMode::Tenant)
        .await
        .expect("put ok");
    let calls = stub.put_calls.lock();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "k");
    assert_eq!(calls[0].1, b"v");
    assert_eq!(calls[0].2, SharingMode::Tenant);
}

#[tokio::test]
async fn delete_maps_not_found_to_ok() {
    let stub = Arc::new(StubMutator::default());
    *stub.delete_response.lock() = Some(Err(CredStoreError::NotFound));
    let ctx = build_system_ctx(Uuid::nil());
    let writer = CredStoreWriter::new(stub, ctx);
    let key = SecretRef::new("k").expect("valid SecretRef");
    writer
        .delete(&key)
        .await
        .expect("NotFound MUST be mapped to Ok");
}

#[tokio::test]
async fn delete_passes_other_errors_through() {
    let stub = Arc::new(StubMutator::default());
    *stub.delete_response.lock() = Some(Err(CredStoreError::service_unavailable("nope")));
    let ctx = build_system_ctx(Uuid::nil());
    let writer = CredStoreWriter::new(stub, ctx);
    let key = SecretRef::new("k").expect("valid SecretRef");
    let err = writer
        .delete(&key)
        .await
        .expect_err("non-NotFound MUST surface");
    assert!(matches!(err, CredStoreError::ServiceUnavailable { .. }));
}

#[tokio::test]
async fn delete_ok_stays_ok() {
    let stub = Arc::new(StubMutator::default());
    let ctx = build_system_ctx(Uuid::nil());
    let writer = CredStoreWriter::new(stub, ctx);
    let key = SecretRef::new("k").expect("valid SecretRef");
    writer.delete(&key).await.expect("Ok MUST stay Ok");
}
