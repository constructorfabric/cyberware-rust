#![doc = include_str!("../README.md")]
pub mod api;
pub mod error;
pub mod gts;
pub mod maintenance;
pub mod models;
pub mod plugin_api;
#[cfg(feature = "test-util")]
pub mod test_util;
pub mod types;

pub use ::gts::GtsId;
pub use api::CredStoreClientV1;
pub use error::CredStoreError;
pub use gts::{CREDENTIAL_RESOURCE_TYPE, CredStorePluginSpecV1, CredentialV1, SecretTypeTraits};
pub use maintenance::CredStoreMaintenanceV1;
pub use models::{
    Credential, CredentialListItem, CredentialPatch, CredentialStatus, CredentialWrite, Fallback,
    GcReport, InheritanceStatus, OwnerId, PatchField, PutOutcome, PutPrecondition, Secret,
    SecretRef, SecretValue, SharingMode, TenantId, Validator, ValueId, WritePrecondition,
};
pub use plugin_api::CredStorePluginClientV1;
pub use types::{FENCE_KEY_VALUE_ID, SECRET_TYPE_CATALOG, SecretType, SecretTypeDescriptor};
