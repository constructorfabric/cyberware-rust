//! Infrastructure layer root for the `ClickHouse` usage-collector plugin.
//!
//! Provides `ClickHouse`-specific storage implementations and the
//! cluster-backed exclusive coordination lock.  Follows the three-layer
//! DDD-light shape documented in DESIGN.md §1.3: this crate is the
//! infrastructure layer, responsible for all I/O — `ClickHouse` HTTP client
//! lifecycle, schema provisioning, and cluster distributed-lock management.
//!
//! - `storage::pool`, `coordination::lock_manager`: connection pool, schema
//!   migration, and the exclusive per-`gts_id` mutex (cluster
//!   `DistributedLockV1`, profile `usage-collector`) that both the create and
//!   delete paths contend for.
//! - `storage::record_store`: usage-record persistence.
//! - `storage::catalog_store`: usage-type catalog CRUD.
//! - `metrics`: `uc_clickhouse_*` OpenTelemetry instruments.

pub mod coordination;
pub mod metrics;
pub mod storage;
