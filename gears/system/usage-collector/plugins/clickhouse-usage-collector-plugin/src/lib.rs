#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! `ClickHouse` storage backend plugin for the Usage Collector storage Plugin SPI.
//!
//! Implements [`usage_collector_sdk::UsageCollectorPluginV1`] on `ClickHouse` —
//! a columnar OLAP database. Layered DDD-light: [`gear`] performs the GTS
//! registration handshake, [`domain`] holds the SPI adapter and store port
//! traits, and [`infra`] holds the `ClickHouse`-backed implementations.

pub mod gear;

pub use gear::ClickHouseUsageCollectorPlugin;

// === INTERNAL MODULES ===
// Implementation detail of the plugin. Exposed `pub` only so the crate's
// integration tests (separate `tests/*.rs` crates) can construct the stores,
// config, and metrics directly — NOT public API. External consumers depend on
// `ClickHouseUsageCollectorPlugin` and resolve everything else through the
// plugin host. `#[doc(hidden)]` keeps these off the rendered API surface.
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;
