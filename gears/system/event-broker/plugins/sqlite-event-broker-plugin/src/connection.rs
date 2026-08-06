//! Opening the database this backend owns.
//!
//! The event log does not live in the database the platform hands the host
//! gear: that one keeps ingest and delivery metadata, and a topic's events are
//! not metadata. This backend therefore opens the storage its own configuration
//! names, and applies its own tables to it.
//!
//! Nothing raw crosses the boundary either way. `toolkit_db::connect_db`
//! returns the same secure `Db` a gear gets from the platform, and the tables
//! are applied through `toolkit_db`'s migration runner rather than by executing
//! schema statements against a pool.

use std::sync::Arc;

use toolkit_db::{ConnectOpts, DBProvider, DbError};

use crate::options::EventLogPath;

/// The name this backend's migration history is recorded under, in its own
/// database. Distinct from the host gear's, because the two databases are.
const MIGRATION_OWNER: &str = "event-broker-sqlite-backend";

/// Opens the event log at `path` and applies this backend's tables to it.
///
/// # Errors
/// [`StorageBackendError::Unavailable`](event_broker_sdk::StorageBackendError::Unavailable)
/// if the database cannot be opened or its tables cannot be applied. Both are
/// startup conditions an operator has to fix - a path that cannot be created,
/// or a file that is not this backend's.
pub async fn open_event_log(
    path: &EventLogPath,
) -> Result<Arc<DBProvider<DbError>>, event_broker_sdk::StorageBackendError> {
    // A pool of one for an in-memory log, because SQLite gives every
    // connection to `:memory:` a database of its own: a second connection
    // would see neither the tables the first applied nor the events it wrote.
    let opts = if path.is_in_memory() {
        ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        }
    } else {
        ConnectOpts::default()
    };

    let db = toolkit_db::connect_db(&path.dsn(), opts)
        .await
        .map_err(|e| unavailable(path, &format!("opening the event log failed: {e}")))?;
    toolkit_db::migration_runner::run_migrations_for_gear(
        &db,
        MIGRATION_OWNER,
        crate::migrations::migrations(),
    )
    .await
    .map_err(|e| {
        unavailable(
            path,
            &format!("applying the event log's tables failed: {e}"),
        )
    })?;

    Ok(Arc::new(DBProvider::new(db)))
}

fn unavailable(path: &EventLogPath, reason: &str) -> event_broker_sdk::StorageBackendError {
    event_broker_sdk::StorageBackendError::Unavailable {
        reason: reason.to_owned(),
        detail: format!("path: {path}"),
        instance: crate::BACKEND_TYPE.to_owned(),
    }
}
