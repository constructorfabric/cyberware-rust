//! Domain layer: SPI adapter and store port traits.
//!
//! - [`ports`]: `RecordStore` and `CatalogStore` trait declarations.
//! - [`adapter`]: `StorageAdapter` — the single [`UsageCollectorPluginV1`] implementation,
//!   delegating every method to the appropriate port.

pub mod adapter;
pub mod ports;
