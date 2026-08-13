//! `ClickHouse`-backed storage implementations.
//!
//! - `pool`: connection-pool bootstrap and schema migration.
//! - `entity`, `mapper`, `error`, `query`, `record_store`: record persistence.
//! - `catalog_store`: usage-type catalog CRUD.

pub mod catalog_store;
pub mod entity;
pub mod error;
pub mod mapper;
pub mod pool;
pub mod query;
pub mod record_store;
