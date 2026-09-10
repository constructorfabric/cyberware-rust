//! Infrastructure layer for the Types Registry gear
//!
//! Contains storage implementations and adapters.

pub mod cache;
pub mod metrics;
pub mod storage;

pub use storage::{InMemoryGtsRepository, Migrator};
