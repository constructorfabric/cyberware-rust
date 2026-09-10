//! Storage layer of the plugin.

pub mod cursor;
pub mod entity;
pub mod foundation_store;
pub mod migrations;
pub mod quota_mapping;
pub mod quota_store;
pub mod repo;

pub use foundation_store::SqlFoundationStore;
pub use migrations::{Migrator, OUTBOX_TABLE_PREFIX};
pub use quota_store::SqlQuotaStore;
