//! Domain layer: the storage plugin skeleton, its bootstrap, and the store
//! port the SQL adapter implements.

pub mod bootstrap;
pub mod ports;

pub use bootstrap::StoragePlugin;
pub use ports::{FoundationStore, SeedReport, StoreError};
