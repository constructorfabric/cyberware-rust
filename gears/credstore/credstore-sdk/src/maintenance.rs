//! In-process maintenance contract for the periodic garbage-collection job
//! (ADR-0006).
//!
//! Unlike [`crate::CredStoreClientV1`], this trait has no REST address: the
//! job's entry point is this SDK trait, registered in `ClientHub` next to
//! `CredStoreClientV1` by the gear's `init`, and resolved and invoked by
//! whatever the host uses to run it on a schedule — a `gc` subcommand of the
//! application binary under a Kubernetes `CronJob`, or a scheduler gear.
//! Credstore itself knows nothing about scheduling: [`Self::run_gc`] is one
//! bounded pass, safe to invoke as often or as rarely as the host chooses.

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::error::CredStoreError;
use crate::models::GcReport;

/// In-process maintenance entry point for the credential store's periodic
/// garbage collection (ADR-0006).
///
/// `run_gc` is idempotent and concurrency-safe: running it twice in
/// immediate succession is a no-op the second time, and it is safe to invoke
/// concurrently from multiple replicas or to re-run after a partial failure
/// — every step it performs is a claim-then-act operation over durable
/// bookkeeping, never a blind retry of someone else's work.
#[async_trait]
pub trait CredStoreMaintenanceV1: Send + Sync {
    /// Runs one bounded pass of the maintenance job: the expired-row sweep,
    /// then the gc drain and pending-intent reclaim, each in batches
    /// (`gc.batch_size`) until a batch makes no further progress.
    ///
    /// `ctx` is expected to be a system security context — this is an
    /// operator-invoked administrative action, not a per-tenant read or
    /// write, and it is not gated by the PDP (no permission is declared for
    /// it; see `gts::permissions`).
    ///
    /// # Errors
    ///
    /// Returns [`CredStoreError::ServiceUnavailable`] only if listing a
    /// batch itself fails (e.g. the database is unreachable). Failures
    /// cleaning up an individual entry are logged and swallowed — the run
    /// always returns its report, and a failed entry is simply picked up by
    /// a later invocation.
    async fn run_gc(&self, ctx: &SecurityContext) -> Result<GcReport, CredStoreError>;
}
