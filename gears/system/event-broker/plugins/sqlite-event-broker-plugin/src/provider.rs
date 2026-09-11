//! The [`EventBrokerBackendProvider`] implementation for the `SQLite` backend.
//!
//! The production glue the gear dispatches to when an operator names this
//! crate's backend type in a topic's `backend.type`. It implements the SDK
//! trait - so this crate depends on `event-broker-sdk` only, never on the gear.

use std::sync::Arc;

use async_trait::async_trait;
use event_broker_sdk::{EventBrokerBackend, EventBrokerBackendProvider, StorageBackendError};

use crate::backend::SqliteEventBackend;
use crate::options::SqliteBackendOptions;

/// The GTS backend type this crate serves, as a topic's `backend.type` names it.
///
/// Owned here rather than in the gear: a backend type is registered by its own
/// plugin crate, so a second backend brings its own identifier and the gear
/// learns no names. Written out in full rather than built with `gts_id!` because
/// this crate deliberately depends on the SDK alone; the gear compares it
/// against the configured type at wiring, so a malformed one cannot pass
/// silently.
pub const BACKEND_TYPE: &str = "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~";

/// Builds the `SQLite` backend from the settings beside its topic's
/// `backend.type`.
///
/// Carries no state: everything this backend needs is operator configuration,
/// including the database it opens. Nothing is captured from the host gear,
/// because the event log does not live in the gear's database.
pub struct SqliteBackendProvider;

#[async_trait]
impl EventBrokerBackendProvider for SqliteBackendProvider {
    fn backend_type(&self) -> &'static str {
        BACKEND_TYPE
    }

    /// Opens the event log the settings name, applying this backend's tables to
    /// it, and returns a backend over it. Each call opens the database it is
    /// given, so two topics pointed at one file get one connection pool each.
    ///
    /// This is where an unknown key is caught: [`SqliteBackendOptions`] rejects
    /// one rather than ignoring it, which for this backend is the difference
    /// between an event log on disk and one that vanishes with the process.
    async fn build_backend(
        &self,
        settings: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<dyn EventBrokerBackend>, StorageBackendError> {
        let options: SqliteBackendOptions =
            serde_json::from_value(serde_json::Value::Object(settings.clone())).map_err(|e| {
                StorageBackendError::InvalidConfig {
                    detail: format!("{BACKEND_TYPE}: {e}"),
                    instance: BACKEND_TYPE.to_owned(),
                }
            })?;
        Ok(Arc::new(SqliteEventBackend::open(&options.path).await?))
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod provider_tests;
