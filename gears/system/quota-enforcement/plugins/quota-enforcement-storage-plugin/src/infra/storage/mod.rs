//! Storage layer of the plugin.

pub mod entity;
pub mod foundation_store;
pub mod migrations;
pub mod repo;

pub use foundation_store::SqlFoundationStore;
pub use migrations::Migrator;
