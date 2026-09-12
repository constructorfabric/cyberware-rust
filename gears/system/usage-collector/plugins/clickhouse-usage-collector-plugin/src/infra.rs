//! Infrastructure layer root for the `ClickHouse` usage-collector plugin.
//!
//! Provides the `ClickHouse`-specific storage implementations.  Follows the
//! three-layer DDD-light shape documented in DESIGN.md §1.3: this crate is the
//! infrastructure layer, responsible for all I/O — `ClickHouse` HTTP client
//! lifecycle, schema provisioning, and the two stores.
//!
//! - `storage::pool`: connection pool and schema migration.
//! - `storage::record_store`: usage-record persistence.
//! - `storage::catalog_store`: usage-type catalog create / get / list /
//!   delete.
//! - `metrics`: `uc_clickhouse_*` OpenTelemetry instruments.
//!
//! There is no coordination primitive: every write path is a plain
//! read-then-insert whose concurrency semantics rest on
//! `ReplacingMergeTree(version)` plus read-time version resolution (see the
//! store module docs and `storage::query::dedup`).

pub mod metrics;
pub mod storage;
