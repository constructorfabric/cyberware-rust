//! Keeps the specification cache following what `types-registry` holds.
//!
//! `types-registry` commits configuration-seeded entities once, during the
//! platform's `post_init`, and offers no change feed, so the broker re-reads on
//! a cadence instead: a topic or an event type registered after startup becomes
//! resolvable at the next pass, without a restart. Watching for change events,
//! and later a registration-time hook, are the two stages after this one.
//!
//! Driven rather than self-scheduling, the same shape
//! [`crate::infra::workers::RetentionWorker`] already uses: the caller decides
//! the cadence and one pass does one load, so a test forces passes instead of
//! sleeping and hoping a background task ran.
//!
//! Only the ingest role runs this. Exactly one role writes the shared state and
//! the others re-read it, which is what lets a delivery instance serve a topic
//! whose settings appear in no configuration that instance was given.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use types_registry_sdk::TypesRegistryClient;

use crate::config::EventBrokerConfig;

pub struct SpecificationRefreshWorker {
    client: Arc<dyn TypesRegistryClient>,
    db: Arc<toolkit_db::DBProvider<toolkit_db::DbError>>,
    config: EventBrokerConfig,
    tick: Duration,
}

impl SpecificationRefreshWorker {
    /// Four arguments, none of them interchangeable: a registry client, a
    /// database handle, the configuration resolution folds in, and a cadence.
    #[must_use]
    pub fn new(
        client: Arc<dyn TypesRegistryClient>,
        db: Arc<toolkit_db::DBProvider<toolkit_db::DbError>>,
        config: EventBrokerConfig,
        tick: Duration,
    ) -> Self {
        Self {
            client,
            db,
            config,
            tick,
        }
    }

    /// One pass: re-read, re-project, re-resolve, upsert.
    ///
    /// A failure is logged and left for the next pass. The cache still holds
    /// what the previous pass found, which is a better answer than none.
    pub async fn run_once(&self) {
        if let Err(err) =
            crate::infra::specification::bulk_load(&self.client, &self.db, &self.config).await
        {
            tracing::warn!(%err, "specification refresh failed; the cache still holds the last load");
        }
    }

    /// Runs until cancelled, one pass per tick, waiting a tick first.
    ///
    /// The initial load is the caller's: nothing may serve a request against an
    /// empty cache, so `serve()` awaits one pass before anything is wired and
    /// this loop picks up from the next tick. Passing straight away here would
    /// re-read what was just read.
    pub async fn run(self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                () = tokio::time::sleep(self.tick) => {}
                () = shutdown.cancelled() => return,
            }
            if shutdown.is_cancelled() {
                return;
            }
            self.run_once().await;
        }
    }

    /// Spawns [`Self::run`]. The handle is returned so a shutdown can join it
    /// rather than leaving a load in flight against a closing pool.
    #[must_use]
    pub fn spawn(self, shutdown: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(self.run(shutdown))
    }
}

#[cfg(test)]
#[path = "specification_refresh_tests.rs"]
mod specification_refresh_tests;
