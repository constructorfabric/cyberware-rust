//! Typed repositories over the Foundation entities.
//!
//! Six land ahead of the paths that call them, and for the same reason: each
//! carries an invariant rather than a caller convention. The pin-frontier
//! repository's `advance` is forward-only in SQL; the plan repository's draft
//! edits are compare-and-swaps in SQL, with the row-version bump inside the same
//! statement that matches on the version the caller read; the idempotency gate's
//! at-most-once guarantee **is** an `INSERT... ON CONFLICT DO NOTHING`; the
//! price repository's row and band set are one transaction, because a row whose
//! geometry can land a moment late is a row that is briefly wrong; the plan
//! shape repository replaces a revision's phase chain wholesale under the
//! **revision's** entity tag, because a child set with a tag of its own would
//! let two authors edit one draft and both satisfy their precondition; the
//! policy repository resolves a tenant's authoring caps **against the deployment
//! defaults**, so no caller can read a per-tenant cap without the ratified value
//! behind it. None of those guards survives being reimplemented per call site —
//! that is what makes them repositories and not helpers.
//!
//! The remaining tables get their repositories with the paths that write them —
//! a repository nothing calls is dead code, and dead code fails CI here.
//! Three of them arrive together with the publish commit — `pricing_audit_log`,
//! `pricing_outbox` and `pricing_catalog_version_ref` — because that is the
//! first path that has an actor, a subject and a transaction to commit inside
//! of. All three are shaped differently from the six above and deliberately so:
//! they take a **runner** rather than a provider, because a record, an event or
//! a pending version handle that could commit separately from the mutation it
//! describes is evidence of something that may not have happened (D-14 for the
//! audit row; the outbox's own "an event exists if and only if its commit
//! happened"; a dangling pending ref that trips the commit-overdue alarm for a
//! publish that never occurred). [`idempotency_repo`] set that precedent for the
//! same reason.
//!
//! [`read_model_repo`] is the fourth runner-taking one, and its reason is
//! sharper still: D-136 requires the pin frontier to advance **in the
//! transaction that sets the last outstanding `warm_completed` marker** of the
//! frontier's next version in order, so the delta write and the advance are one
//! transaction by rule rather than by preference — and a repository holding a
//! provider could not join it, `Db::conn()` being refused outright inside an
//! open transaction.

pub mod approval_repo;
pub mod audit_repo;
pub mod bulk_repo;
pub mod bundle_repo;
pub mod catalog_version_ref_repo;
pub mod group_membership_repo;
pub mod idempotency_repo;
pub mod migration_repo;
pub mod outbox_repo;
pub mod overlay_repo;
pub mod pin_frontier_repo;
pub mod plan_repo;
pub mod plan_shape_repo;
pub mod policy_repo;
pub mod price_repo;
pub mod read_model_repo;
pub mod repricing_journal_repo;
pub mod synthesis_repo;
pub mod taxonomy_repo;
pub mod threshold_repo;
pub mod window_repo;

use time::OffsetDateTime;

use crate::domain::instant;
use crate::infra::storage::RepoError;

use crate::domain::instant::format_rfc3339;
pub use approval_repo::{ApprovalRecord, NewApproval};
pub use audit_repo::NewAuditEntry;
pub use bulk_repo::{BulkOperationRecord, BulkRepo, NewBulkOperation};
pub use bundle_repo::{
    BundleComponentDraft, BundleRecord, BundleRepo, CompositionDraft, NewBundle,
};
pub use catalog_version_ref_repo::PendingVersionRow;
pub use group_membership_repo::{MembershipRow, NewMembership};
pub use idempotency_repo::{ClaimOutcome, IdempotencyGate};
pub use migration_repo::{MigrationRecord, NewMigration};
pub use outbox_repo::{
    NewOutboxEvent, PlanMigrationScheduledPayload, PlanPublishDegradedPayload,
    PlanPublishedPayload, PlanRetiredPayload, PriceUpdatedPayload, PriceWindowTransitionPayload,
    WindowMutationEvent,
};
pub use overlay_repo::{NewOverlay, OverlayRecord, OverlayRepo};
pub use pin_frontier_repo::PinFrontierRepo;
pub use plan_repo::{NewPlanDraft, PlanRepo};
pub use plan_shape_repo::PlanShapeRepo;
pub use policy_repo::{AuthoringPolicy, PolicyObjectRepo};
pub use price_repo::{NewPriceDraft, PriceRepo};
pub use read_model_repo::NewDelta;
pub use synthesis_repo::{NewProvenance, ProvenanceRecord};
pub use threshold_repo::{StoredVersion, ThresholdEntryRow};
pub use window_repo::{NewWindow, WindowRecord};

/// Refuse an authored instant finer than the millisecond quantum (D-144).
///
/// Here rather than in each repository because several of them store instants an
/// operator authored — `grandfatherUntil`, `availableFrom`/`availableTo` — and
/// the quantum is one rule. The predicate itself stays in
/// [`crate::domain::instant`]: the resolution the catalog compares at is a
/// domain fact, and this is only the storage boundary refusing to write past it.
///
/// # Its callers are the tables holding an authored instant, and that is checkable
///
/// A rule stated over an unnamed set cannot be checked against the set — "both of
/// them" hides a repository that stores an authored instant without calling this.
/// The callers are `plan_repo`
/// (`availableFrom`/`availableTo`), `price_repo` (`grandfatherUntil`),
/// `window_repo` and `group_membership_repo` and `threshold_repo`
/// (`effectiveFrom`/`effectiveTo`), `overlay_repo` (the overlay interval and a
/// line's `cohort`), `migration_repo` (`effectiveAt`) and `synthesis_repo`
/// (`snapshotInstant`) — every table with such a column, which is the invariant to
/// re-derive by grep rather than a list to trust.
///
/// **What is outside it is machine-generated, not merely uncompared.** `created_at`,
/// the audit chain, outbox timestamps, `pricing_migration.announced_at` and a
/// window activation's flip instant are all minted from `OffsetDateTime::now_utc()`, which carries
/// sub-millisecond precision — [`window_repo::transition`] measured what applying
/// the quantum there does: it refuses every write. `domain::instant` states the
/// same exclusion from the domain side.
///
/// The columns will not do it for us. `timestamptz` holds microseconds and
/// `SQLite`'s text rendering holds whatever it is handed, so a finer instant
/// persists in silence and is then matched for equality against one produced at
/// the quantum in another gear.
///
/// `None` is nothing authored, which is not a precision fault.
///
/// # Errors
/// [`RepoError::TimestampPrecisionExceeded`] naming `field` and the instant, so
/// the author corrects one value rather than resubmitting and guessing.
pub(crate) fn check_authored_instant(
    field: &str,
    at: Option<OffsetDateTime>,
) -> Result<(), RepoError> {
    let Some(at) = at else {
        return Ok(());
    };
    if instant::is_quantized(at) {
        return Ok(());
    }
    Err(RepoError::TimestampPrecisionExceeded {
        field: field.to_owned(),
        value: format_rfc3339(at),
    })
}
