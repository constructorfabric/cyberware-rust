//! # `SQLite` event-broker plugin
//!
//! The durable storage backend for the `event-broker` gear: the append-only
//! `(topic, partition)` event log, the per-partition bookkeeping that assigns
//! sequences and dedups outbox retries, and the retention pass that keeps every
//! partition bounded.
//!
//! Implements `event_broker_sdk::EventBrokerBackend` and depends on the SDK
//! only, never on the gear. Not a `RunnableCapability` and not a `Gear`:
//! following the cluster plugins, it exposes a provider the host gear injects
//! at wiring.
//!
//! The backend owns no task and no timer. Its retention pass is a trait method
//! the gear drives on its own tick, so a test forces a pass deterministically
//! instead of sleeping and hoping a background thread ran.
//!
//! The database it writes to is its own. The one the platform hands the host
//! gear keeps ingest and delivery metadata; a topic's events are not metadata,
//! so this backend opens the storage its options name - a file, or `:memory:`
//! for a log that lives no longer than the process - and applies its own tables
//! to it.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

mod backend;
mod connection;
pub mod entity;
mod error;
mod migrations;
mod options;
mod provider;
mod sizing;

#[cfg(test)]
mod footprint_tests;
#[cfg(test)]
mod test_support;

pub use backend::SqliteEventBackend;
pub use options::{EventLogPath, SqliteBackendOptions};
pub use provider::{BACKEND_TYPE, SqliteBackendProvider};
