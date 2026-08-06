//! Event Broker module: `Ingest`/`Delivery`/`Dispatcher` composite, wired
//! per deployment mode by [`module::EventBrokerModule`].
//!
//! See `docs/DESIGN.md` (module tree, §3.8 Deployment Topology, §4.1
//! Deployment Modes) and `docs/ADR/0007-service-decomposition.md`.

pub mod api;
pub mod config;

#[cfg(test)]
mod config_tests;
pub mod domain;
pub mod infra;
pub mod module;

#[cfg(test)]
pub(crate) mod test_support;

pub use config::{DeploymentMode, EventBrokerConfig};
pub use module::EventBrokerModule;
