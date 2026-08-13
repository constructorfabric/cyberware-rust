//! Cluster-backed coordination primitives.
//!
//! Provides the per-`gts_id` exclusive coordination lock that closes the
//! concurrent-reference race described in DESIGN.md §3.5 and §3.6.
//!
//! Both record create and catalog delete acquire the same exclusive lock name
//! for a given `gts_id`, serializing those paths against each other (and
//! serializing concurrent creates for the same `gts_id`).
//!
//! This module owns only the `LockManager` construction and the acquire
//! APIs; lock *usage* lives elsewhere:
//! - Exclusive lock acquisition at ingest call sites → Record Store
//!   (`cpt-cf-uc-ch-plugin-feature-record-persistence`).
//! - Exclusive lock acquisition at delete call sites → Catalog Store
//!   (`cpt-cf-uc-ch-plugin-feature-usage-type-catalog`).

pub mod lock_manager;
