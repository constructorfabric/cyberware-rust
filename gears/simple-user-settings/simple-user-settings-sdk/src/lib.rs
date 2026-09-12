//! Settings SDK
//!
//! This crate provides the public API for the settings gear:
//! - `SimpleUserSettingsClientV1` trait for inter-gear communication
//! - Model types (`SimpleUserSettings`, `SimpleUserSettingsPatch`)
//!
//! Trait methods return `Result<_, CanonicalError>` — this SDK is the
//! Pattern 1 reference for ADR 0005 (canonical-at-boundary with no
//! typed projection). Its consumer dispatch needs do not warrant the
//! extra projection layer that oagw-sdk ships; callers either
//! propagate `CanonicalError` directly or `match` on the canonical
//! categories themselves.
//!
//! Consumers obtain the client from `ClientHub`:
//! ```ignore
//! let client = hub.get::<dyn SimpleUserSettingsClientV1>()?;
//! let settings = client.get_settings(&ctx).await?;
//! ```

#![forbid(unsafe_code)]

pub mod api;
pub mod models;
pub mod owner;

pub use api::SimpleUserSettingsClientV1;
pub use models::{SimpleUserSettings, SimpleUserSettingsPatch, SimpleUserSettingsUpdate};
pub use owner::SettingsOwnerResolver;
