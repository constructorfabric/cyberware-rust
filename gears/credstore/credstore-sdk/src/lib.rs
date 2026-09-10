#![doc = include_str!("../README.md")]
pub mod api;
pub mod error;
pub mod gts;
pub mod models;
pub mod plugin_api;
#[cfg(feature = "test-util")]
pub mod test_util;
pub mod types;

pub use ::gts::GtsId;
pub use api::CredStoreClientV1;
pub use error::CredStoreError;
pub use gts::{CredStorePluginSpecV1, SECRET_RESOURCE_TYPE, SecretTypeTraits, SecretV1};
pub use models::{
    ExpiryWrite, GetSecretResponse, OwnerId, SecretRef, SecretValue, SharingMode, TenantId,
    ValueId, WriteOptions, WritePrecondition,
};
pub use plugin_api::CredStorePluginClientV1;
pub use types::{FENCE_KEY_VALUE_ID, SECRET_TYPE_CATALOG, SecretType, SecretTypeDescriptor};
