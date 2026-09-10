//! Domain layer: the storage plugin, its bootstrap, the Quota primitives it
//! forwards, and the store ports the SQL adapters implement.

pub mod bootstrap;
pub mod ports;
pub mod quotas;

pub use bootstrap::StoragePlugin;
pub use ports::{Actor, FoundationStore, QuotaStore, SeedReport, StoreError};
