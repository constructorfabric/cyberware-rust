//! Repository for a `PriceOverlay` and its revision chain — `pricing_price_overlay`
//! plus the two child tables that version with it (`design/09-price-overlays.md`
//! §6, D-42, D-92, D-107).
//!
//! # The overlay carries its own revision chain, unlike a bundle
//!
//! [`bundle_repo`](super::bundle_repo)'s composition rides the **plan's**
//! revisions and takes the plan revision's entity tag. An overlay has no plan:
//! it is a separate object evaluated downstream, and D-92 gives it a chain of
//! its own keyed `(price_overlay_id, revision)`. So this repository owns the
//! whole discipline rather than borrowing one — the open, the copy forward, the
//! compare-and-swap and the publish flip are all here.
//!
//! # The line set is replaced wholesale, never merged
//!
//! Every D-42 rule is a property of the **set** — "≥ 1 line", the duplicate-key
//! rule, the most-specific resolution, the per-currency coverage walk — and a
//! partial update leaves no set the rules could evaluate. It is `PriceRepo`'s
//! band precedent and `BundleRepo`'s composition precedent, for the reason both
//! give.
//!
//! # The copy forward preserves `line_id`, which is the whole of D-92's clause
//!
//! [`OverlayRepo::open_revision`] copies each line onto the successor revision
//! **under its own id**. That is what *"copy-on-new-revision with stable line
//! identity where unchanged"* means, and it is only expressible because the
//! line's key is `(line_id, overlay_revision)` — see `pricing_price_overlay_line`'s module
//! doc for why §6's literal `PK line_id` is not buildable beside that clause.
//!
//! # The publish is one commit, and it has to be
//!
//! §6: *"the submit/commit publishes that revision and flips its predecessor
//! `published -> superseded` **in the same commit**"*. Two commits would leave a
//! window in which the overlay has two published revisions — which the partial
//! precedence index would refuse outright, so the intermediate state is not
//! merely wrong but unreachable. [`OverlayRepo::publish_revision`] does both
//! inside one transaction.
//!
//! **The predecessor is superseded first, and the order matters more than the
//! index argument suggests.** Measured by reversing it: eight cases redden, not
//! the one about superseding. The predecessor is found *by state* — the
//! published revision of this overlay — so a lookup made after the successor's
//! flip finds **the row just published** and supersedes it, leaving the overlay
//! with no published revision at all. The partial index is the second reason;
//! the first is that the query has no way to tell the two apart once both are
//! `published`.
//!
//! # All four mutations write an audit record, on the overlay's own chain
//!
//! D-14 and `inst-rb-audit`: [`record_overlay_mutation`] appends to
//! `pricing_audit_log` **inside** each mutation's transaction, because a record
//! that could commit separately from the mutation it describes is evidence of
//! something that may not have happened.
//!
//! The chain is [`audit_repo::overlay_chain`] and not any plan's. D-135 keys a
//! segment on the audited subject's *aggregate*; S5 §6 lists an overlay as one in
//! its own right, and it has to be, because an overlay is not a plan and has no
//! plan — borrowing `PlanRevision` would put two aggregates on one hash chain,
//! which then verifies perfectly while telling an auditor something false. An
//! overlay's lines may target several plans or none at all
//! ([`LineKey::list_default`]), so there is not even a well-defined plan to
//! borrow.
//!
//! The pull towards the opposite is worth naming, because it is structural rather
//! than careless: giving `AuditSubjectKind` an overlay member makes two exhaustive
//! `match`es non-exhaustive, and one of them is in `infra::approval`. A strand
//! fenced out of that file carries its [`AuditStamp`] to a `let _ = stamp;`
//! instead, and the plane writes nothing at all — a plain regression against D-14
//! on an always-material subject.
//!
//! What the register sized as *"an addition at four call sites, not a signature
//! sweep"* was that plus one more thing it did not see: `AuditSubjectKind::ALL`
//! drives `sqlite_approval_repo::every_subject_kind_d158_declares_is_storable_on_the_mirror`,
//! so a fifth member obliges `chk_pricing_approval_subject_kind` to admit it —
//! D-158's *"extended together"*, arriving as `chk_pricing_approval_subject_kind` and a `SQLite`
//! table rebuild.
//!
//! # What the store answers and what the pipeline answers
//!
//! Two of §5's overlay codes are typed **409** rather than as architectural
//! 422s: `PRECEDENCE_DUPLICATE` and `OVERLAY_INTERVAL_OVERLAP`. Both have a
//! *checking* half in [`crate::domain::overlay_rules`], which puts them in the
//! one-pass report an author remediates from, and `PRECEDENCE_DUPLICATE` has a
//! *constraint* half here — `uq_pricing_price_overlay_precedence`, which is what
//! catches two submits that both read a free slot before either wrote. The two
//! halves answer the same code and the same remedy, which is
//! [`RepoError::PendingKeyHeld`]'s arrangement one plane over.

use std::collections::{BTreeMap, BTreeSet};

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, Order};
use time::OffsetDateTime;
use toolkit_db::odata::sea_orm_filter::paginate_odata;
use toolkit_db::secure::{
    AccessScope, DBRunner, DbTx, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt,
    SecureUpdateExt,
};
use toolkit_db::{DBProvider, DbError};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use bss_pricing_sdk::odata::OverlayFilterField;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::CurrencyCode;
use crate::domain::overlay::OverlayRevision;
use crate::domain::overlay::{
    Adjustment, AdjustmentKind, AmountSet, Disclosure, LineKey, Magnitude, MagnitudeKind,
    OverlayInterval, OverlayLifecycle, OverlayLine, ScopeClass, ScopeSelector, ScopeValue,
    TargetRef, TargetSku, TaxBasis,
};
use crate::domain::overlay_rules::{CrossClassTie, OverlayWorld, PublishedLineInterval};
use crate::domain::scope_key::{PlanId, PriceEligibility};
use crate::domain::taxonomy::TaxonomyState;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::{
    brand_taxonomy, customer_group_taxonomy, org_tier_taxonomy, partner_taxonomy, plan, price,
    price_overlay, price_overlay_line, price_overlay_line_amount, region_taxonomy,
};
use crate::infra::storage::odata_mapping::{
    LIST_LIMIT_CFG, OdataPageError, OverlayODataMapper, map_odata_err, query_with_default_order,
    query_with_unique_order,
};
use crate::infra::storage::repo::plan_repo::{read_token, tx_failure};
use crate::infra::storage::repo::{NewAuditEntry, audit_repo, check_authored_instant};

// ---------------------------------------------------------------------------
// The authoring surface's types.
// ---------------------------------------------------------------------------

/// An overlay to create, with its first revision's lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewOverlay {
    /// The overlay's identity, minted by the caller.
    pub price_overlay_id: Uuid,
    /// The tenant, checked against the caller's scope on the way in.
    pub tenant_id: Uuid,
    /// Scope class and value.
    pub scope: ScopeSelector,
    /// L2's explicit precedence.
    pub precedence: i32,
    /// The overlay's own dating.
    pub interval: OverlayInterval,
    /// L5's declared basis — already past
    /// [`check_tax_basis_declared`](crate::domain::overlay_rules::check_tax_basis_declared).
    pub tax_basis: TaxBasis,
    /// L6's exposure flag.
    pub disclosure: Disclosure,
    /// The plans the lines may target.
    pub target_ref: TargetRef,
}

/// An overlay revision as the store holds it, lines and all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayRecord {
    /// The overlay's identity.
    pub price_overlay_id: Uuid,
    /// Which revision this is.
    pub revision: u64,
    /// Its state.
    pub lifecycle_state: OverlayLifecycle,
    /// Scope class and value.
    pub scope: ScopeSelector,
    /// Its precedence.
    pub precedence: i32,
    /// Its own dating.
    pub interval: OverlayInterval,
    /// Its declared basis.
    pub tax_basis: TaxBasis,
    /// Its exposure flag.
    pub disclosure: Disclosure,
    /// The plans its lines may target.
    pub target_ref: TargetRef,
    /// The concurrency token the next edit must present.
    pub row_version: i64,
    /// Its adjustment lines, in a stable order.
    pub lines: Vec<OverlayLine>,
}

impl OverlayRecord {
    /// This revision's **content**, as an approval unit pins it (D-225).
    ///
    /// Everything but `row_version`, and the omission is the decision:
    /// `OverlayRevision`'s doc has the argument at full strength — a pin answers
    /// *"is this the content the reviewer saw"*, and a concurrency token moves for
    /// reasons the content did not, so pinning it would fail a decision for a reason
    /// no reviewer can see or act on.
    #[must_use]
    pub fn content(&self) -> OverlayRevision {
        OverlayRevision {
            price_overlay_id: self.price_overlay_id,
            revision: self.revision,
            lifecycle_state: self.lifecycle_state,
            scope: self.scope.clone(),
            precedence: self.precedence,
            interval: self.interval,
            tax_basis: self.tax_basis,
            disclosure: self.disclosure,
            target_ref: self.target_ref.clone(),
            lines: self.lines.clone(),
        }
    }
}

/// Repository over `pricing_price_overlay` and its two child tables.
#[derive(Clone)]
pub struct OverlayRepo {
    db: DBProvider<DbError>,
}

impl OverlayRepo {
    /// Build over one database provider.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Create the overlay at revision `0`, in `draft`, with its lines.
    ///
    /// One transaction: the header, every line and every per-currency value. The
    /// audit record that belongs in it is owed — see the module doc.
    ///
    /// # Errors
    /// [`RepoError::TimestampPrecisionExceeded`] when a line's `cohort` or either
    /// bound of the overlay's own interval is finer than the millisecond quantum
    /// (D-144; see [`write_lines`]); [`RepoError::Db`] on a scope or storage failure.
    ///
    /// **Not [`RepoError::OverlayPrecedenceHeld`]**, although a precedence is
    /// written here: `uq_pricing_price_overlay_precedence` is partial on
    /// `lifecycle_state = 'published'` and this inserts a `draft`, so the index
    /// cannot fire on the way in. Two drafts may share a precedence, and the
    /// second one to **publish** is the one refused — see
    /// [`OverlayRepo::publish_revision`].
    ///
    /// The three statements live in [`create_on`], which is this method's body on
    /// a runner the caller owns; this form opens the transaction they need. The two
    /// paths therefore issue the same statements rather than two copies of them.
    pub async fn create(
        &self,
        scope: &AccessScope,
        new: NewOverlay,
        lines: Vec<OverlayLine>,
        stamp: AuditStamp,
    ) -> Result<u64, RepoError> {
        // The overlay's own interval is an authored, published, **compared**
        // instant pair — `OverlayInterval::intersects` is what `check_dating`
        // decides collisions with — so it meets D-144's quantum here, exactly as
        // `window_repo::schedule` gates the identically-shaped pair one module
        // over. Before the transaction opens: a precision fault is a property of
        // the request alone and needs no row read to decide.
        check_authored_instant("effectiveFrom", new.interval.from)?;
        check_authored_instant("effectiveTo", new.interval.to)?;
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<u64, RepoError, _>(move |txn| {
                Box::pin(async move { create_on(txn, &scope, new, lines, stamp).await })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Open a successor **draft** revision, copying the published revision's
    /// header and its whole line set forward.
    ///
    /// The copy preserves each `line_id` — D-92's *"stable line identity where
    /// unchanged"*, and the reason the line table's key carries the revision.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when the overlay has no published revision;
    /// [`RepoError::OverlayOpenDraftExists`] when one is already open;
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn open_revision(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_overlay_id: Uuid,
        stamp: AuditStamp,
    ) -> Result<u64, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<u64, RepoError, _>(move |txn| {
                Box::pin(async move {
                    // The read is the explanatory path and the partial index is
                    // the guarantee — D-148's read-then-index arrangement, here
                    // for a uniqueness the caller can be told about in words.
                    if let Some(open) = revision_in_state(
                        txn,
                        &scope,
                        tenant_id,
                        price_overlay_id,
                        OverlayLifecycle::Draft,
                    )
                    .await?
                    {
                        return Err(
                            match stored_counter(price_overlay_id, "revision", open.revision) {
                                Ok(revision) => RepoError::OverlayOpenDraftExists {
                                    price_overlay_id: price_overlay_id.to_string(),
                                    revision,
                                },
                                Err(corrupt) => corrupt,
                            },
                        );
                    }
                    let published = revision_in_state(
                        txn,
                        &scope,
                        tenant_id,
                        price_overlay_id,
                        OverlayLifecycle::Published,
                    )
                    .await?
                    .ok_or_else(|| RepoError::NotFound {
                        subject: "published price overlay".to_owned(),
                        id: price_overlay_id.to_string(),
                    })?;

                    // **`max(revision) + 1`, not `published.revision + 1`.**
                    // `PlanRepo` mints the same way. The published row is not the
                    // chain's high-water mark in general — a `superseded`
                    // predecessor can outrank it — so deriving from it is deriving
                    // from the wrong row even where the two happen to agree.
                    //
                    // **A true high-water read, and D-231 is what makes it one.**
                    // With a discarded draft leaving by DELETE — three states and
                    // no tombstone — the number it consumed is gone from the
                    // table and `max` cannot see it, whatever this line says. The
                    // `abandoned` state removed DELETE as an exit, so the
                    // discarded row is still here and still counted:
                    // `sqlite_overlay_repo::an_abandoned_revision_number_is_never_re_minted`
                    // is the case, and it asserts the successor is **new**.
                    let successor = highest_revision(txn, &scope, tenant_id, price_overlay_id)
                        .await?
                        .saturating_add(1);
                    let copy = price_overlay::ActiveModel {
                        price_overlay_id: Set(published.price_overlay_id),
                        revision: Set(successor),
                        tenant_id: Set(published.tenant_id),
                        lifecycle_state: Set(OverlayLifecycle::Draft.as_str().to_owned()),
                        scope_class: Set(published.scope_class.clone()),
                        scope_value: Set(published.scope_value.clone()),
                        precedence: Set(published.precedence),
                        effective_from: Set(published.effective_from),
                        effective_to: Set(published.effective_to),
                        tax_basis: Set(published.tax_basis.clone()),
                        disclosure: Set(published.disclosure.clone()),
                        target_ref: Set(published.target_ref.clone()),
                        // The successor starts its own tag sequence: a caller
                        // holding the predecessor's tag is editing a revision
                        // that is now frozen, and must be told so rather than
                        // silently succeeding here.
                        row_version: Set(0),
                    };
                    insert_overlay(txn, &scope, copy).await?;
                    copy_lines(
                        txn,
                        &scope,
                        price_overlay_id,
                        tenant_id,
                        published.revision,
                        successor,
                    )
                    .await?;
                    // `create` rather than `update`: `open_revision` writes a new
                    // revision **row** and leaves the published one exactly as it
                    // was, so an auditor reading `update` here would look for a
                    // change to the revision they were already holding.
                    record_overlay_mutation(
                        txn,
                        &scope,
                        tenant_id,
                        price_overlay_id,
                        successor,
                        AuditAction::Create,
                        stamp,
                    )
                    .await?;
                    stored_counter(price_overlay_id, "revision", successor)
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Replace an open draft revision's whole line set, under the caller's row
    /// version.
    ///
    /// The order is: resolve the revision under a compare-and-swap, drop its
    /// lines, insert the submitted set, record. The swap carries
    /// `lifecycle_state = 'draft'` as well as the version, so a caller aiming at
    /// a frozen revision is refused by the statement rather than by a trigger.
    ///
    /// # Errors
    /// [`RepoError::StaleRowVersion`] / [`RepoError::NotDraft`] /
    /// [`RepoError::NotFound`] as [`refuse_edit`] resolves them;
    /// [`RepoError::TimestampPrecisionExceeded`] when a submitted line's `cohort`
    /// is finer than the millisecond quantum (D-144; see [`write_lines`]);
    /// [`RepoError::Db`] on a scope or storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "the compare-and-swap needs the whole coordinate — scope, tenant, \
                  overlay, revision and the version read — beside the line set and \
                  the audit stamp. Bundling them into a struct would put six values \
                  that are never carried together anywhere else into a type that \
                  exists only to satisfy a count; `BundleRepo::replace_composition` \
                  takes the same eight for the same reason."
    )]
    pub async fn replace_lines(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_overlay_id: Uuid,
        revision: u64,
        expected: i64,
        lines: Vec<OverlayLine>,
        stamp: AuditStamp,
    ) -> Result<i64, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<i64, RepoError, _>(move |txn| {
                Box::pin(async move {
                    let Ok(number) = i64::try_from(revision) else {
                        return Err(RepoError::NotFound {
                            subject: "price overlay revision".to_owned(),
                            id: format!("{price_overlay_id}/{revision}"),
                        });
                    };
                    let moved = price_overlay::Entity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .col_expr(
                            price_overlay::Column::RowVersion,
                            Expr::col(price_overlay::Column::RowVersion).add(1),
                        )
                        .filter(
                            Condition::all()
                                .add(price_overlay::Column::TenantId.eq(tenant_id))
                                .add(price_overlay::Column::PriceOverlayId.eq(price_overlay_id))
                                .add(price_overlay::Column::Revision.eq(number))
                                .add(price_overlay::Column::RowVersion.eq(expected))
                                .add(
                                    price_overlay::Column::LifecycleState
                                        .eq(OverlayLifecycle::Draft.as_str()),
                                ),
                        )
                        .exec(txn)
                        .await
                        .map_err(|e| RepoError::Db(format!("bump pricing_price_overlay: {e}")))?
                        .rows_affected;
                    if moved == 0 {
                        // The swap is the guard; this read only decides which of
                        // the three refusals the caller is owed.
                        return Err(refuse_edit(
                            txn,
                            &scope,
                            tenant_id,
                            price_overlay_id,
                            number,
                            expected,
                        )
                        .await);
                    }

                    drop_lines(txn, &scope, price_overlay_id, tenant_id, number).await?;
                    write_lines(txn, &scope, price_overlay_id, tenant_id, number, &lines).await?;
                    record_overlay_mutation(
                        txn,
                        &scope,
                        tenant_id,
                        price_overlay_id,
                        number,
                        AuditAction::Update,
                        stamp,
                    )
                    .await?;
                    Ok(expected + 1)
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Publish a draft revision and supersede its predecessor, **in one commit**.
    ///
    /// §6 requires the pair to be atomic, and the partial precedence index makes
    /// the intermediate state unreachable rather than merely wrong: two
    /// published revisions of one overlay on one `(class, precedence)` is
    /// exactly what that index refuses.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when the revision is not an open draft;
    /// [`RepoError::OverlayPrecedenceHeld`] when the index refuses the flip;
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn publish_revision(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_overlay_id: Uuid,
        revision: u64,
        stamp: AuditStamp,
    ) -> Result<(), RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<(), RepoError, _>(move |txn| {
                Box::pin(async move {
                    publish_revision_on(txn, &scope, tenant_id, price_overlay_id, revision, stamp)
                        .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Discard an open draft revision — the **flip**, never a deletion (D-231).
    ///
    /// This is the sanctioned discard, and before D-231 the overlay plane had
    /// none: no repository method removed a revision at all, and the only path
    /// that discarded one was a raw `DELETE` in a test. The store now refuses that
    /// `DELETE` on both engines, so this is the one way out of a draft that is not
    /// a publish.
    ///
    /// **The number stays consumed.** That is the whole point: the row survives as
    /// a tombstone, so `open_revision`'s `max(revision) + 1` mints a genuinely new
    /// number and a client holding the discarded revision's entity tag can no
    /// longer have its stale `If-Match` match a *different* revision under the
    /// same overlay identity.
    ///
    /// **The lines are dropped before the flip, and the order is load-bearing.**
    /// `abandoned` is not `draft`, and the three
    /// `pricing_price_overlay_line`/`…_line_amount` delete triggers each refuse a
    /// child row whose parent revision is not a draft — so a flip issued first
    /// would leave the line set undeletable and the tombstone carrying content it
    /// is not supposed to have. `PlanRepo::abandon_draft` drops its children in
    /// the same order for the same reason.
    ///
    /// **Abandoning revision 0 of a never-published overlay spends the identity**,
    /// and that is stated rather than guarded against. `open_revision` mints a
    /// successor from the *published* revision, so an overlay whose only revision
    /// was discarded has no revision to open from and the identity is terminal.
    /// That is the correct outcome for an overlay no consumer ever saw — nothing
    /// pinned it, nothing resolved it, and a fresh `create` costs one id — and
    /// inventing a resurrection path would be surface the design set has not
    /// asked for. Named here because the shape is a trap for a caller who reads
    /// "discard" as "undo".
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when the revision is not this overlay's open draft;
    /// [`RepoError::StaleRowVersion`] when `expected` is not the revision's tag;
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn abandon_draft(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_overlay_id: Uuid,
        revision: u64,
        expected: i64,
        stamp: AuditStamp,
    ) -> Result<(), RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<(), RepoError, _>(move |txn| {
                Box::pin(async move {
                    let Ok(number) = i64::try_from(revision) else {
                        return Err(RepoError::NotFound {
                            subject: "price overlay revision".to_owned(),
                            id: format!("{price_overlay_id}/{revision}"),
                        });
                    };
                    // **The compare-and-swap is the guard**, exactly as it is in
                    // `replace_lines` one path over. It ran without an `expected`
                    // for one wave (D-255) -- a caller holding a stale tag could
                    // discard a draft someone else had since edited, and the
                    // discard is not a remediable act. The bump also happens
                    // *before* the drop below, so a write is never issued on the
                    // way to saying no -- `publish_revision_on`'s ordering.
                    let moved = price_overlay::Entity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .col_expr(
                            price_overlay::Column::RowVersion,
                            Expr::col(price_overlay::Column::RowVersion).add(1),
                        )
                        .filter(
                            Condition::all()
                                .add(price_overlay::Column::TenantId.eq(tenant_id))
                                .add(price_overlay::Column::PriceOverlayId.eq(price_overlay_id))
                                .add(price_overlay::Column::Revision.eq(number))
                                .add(price_overlay::Column::RowVersion.eq(expected))
                                .add(
                                    price_overlay::Column::LifecycleState
                                        .eq(OverlayLifecycle::Draft.as_str()),
                                ),
                        )
                        .exec(txn)
                        .await
                        .map_err(|e| RepoError::Db(format!("bump pricing_price_overlay: {e}")))?
                        .rows_affected;
                    if moved == 0 {
                        return Err(refuse_edit(
                            txn,
                            &scope,
                            tenant_id,
                            price_overlay_id,
                            number,
                            expected,
                        )
                        .await);
                    }

                    drop_lines(txn, &scope, price_overlay_id, tenant_id, number).await?;

                    if flip(
                        txn,
                        &scope,
                        tenant_id,
                        price_overlay_id,
                        number,
                        OverlayLifecycle::Abandoned,
                    )
                    .await?
                        == 0
                    {
                        // The read above passed and the swap matched nothing, so a
                        // concurrent publish took the draft in between. The
                        // rollback is the guard; this only picks the sentence.
                        return Err(RepoError::NotFound {
                            subject: "open draft price overlay revision".to_owned(),
                            id: format!("{price_overlay_id}/{revision}"),
                        });
                    }

                    // `Abandon` rather than `Delete`: D-145 minted that token
                    // precisely because the row survives and its number stays
                    // consumed, and an auditor reading `Delete` here would look
                    // for a row that is still there.
                    record_overlay_mutation(
                        txn,
                        &scope,
                        tenant_id,
                        price_overlay_id,
                        number,
                        AuditAction::Abandon,
                        stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// One revision of one overlay, lines and all.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] for a stored token no `CHECK` should have
    /// admitted.
    pub async fn load(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_overlay_id: Uuid,
        revision: u64,
    ) -> Result<Option<OverlayRecord>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("pricing_price_overlay conn: {e}")))?;
        load_on(&conn, scope, tenant_id, price_overlay_id, revision).await
    }

    /// The **published** revision of one overlay, if it has one.
    ///
    /// # Errors
    /// As [`OverlayRepo::load`].
    pub async fn current(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_overlay_id: Uuid,
    ) -> Result<Option<OverlayRecord>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("pricing_price_overlay conn: {e}")))?;
        let Some(row) = revision_in_state(
            &conn,
            scope,
            tenant_id,
            price_overlay_id,
            OverlayLifecycle::Published,
        )
        .await?
        else {
            return Ok(None);
        };
        let lines = read_lines(&conn, scope, price_overlay_id, tenant_id, row.revision).await?;
        record_of(&row, lines).map(Some)
    }

    /// The revision an authoring caller is working with: the open draft, else
    /// the published revision.
    ///
    /// Same rule as the plan plane's `GET /plans/{planId}`: a caller editing an
    /// overlay is working on the draft, and a read that answered the published
    /// revision would hand them a body their next `PATCH` would not match.
    /// `current` stays published-only — a pin and a re-warm must not see a draft.
    ///
    /// # Errors
    /// As [`OverlayRepo::load`].
    pub async fn authoring(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_overlay_id: Uuid,
    ) -> Result<Option<OverlayRecord>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("pricing_price_overlay conn: {e}")))?;
        if let Some(row) = revision_in_state(
            &conn,
            scope,
            tenant_id,
            price_overlay_id,
            OverlayLifecycle::Draft,
        )
        .await?
        {
            let lines = read_lines(&conn, scope, price_overlay_id, tenant_id, row.revision).await?;
            return record_of(&row, lines).map(Some);
        }
        self.current(scope, tenant_id, price_overlay_id).await
    }

    /// Every overlay revision of one tenant, optionally narrowed to one scope
    /// class — the `GET /bss-pricing/v1/price-overlays` read.
    ///
    /// **Every revision, not only the published ones.** This is the admin and
    /// Tariffs read (`price_overlay x read`), and an operator listing overlays
    /// needs to see the draft they are editing. L6's `restricted` flag governs
    /// **consumer-facing** exposure and explicitly does not narrow an operator
    /// read (§3 step 7), so nothing is filtered here on it.
    ///
    /// # Errors
    /// As [`OverlayRepo::load`].
    pub async fn list(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        class: Option<ScopeClass>,
        after: Option<(Uuid, u64)>,
        limit: u64,
    ) -> Result<Vec<OverlayRecord>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("pricing_price_overlay conn: {e}")))?;
        let mut condition = Condition::all().add(price_overlay::Column::TenantId.eq(tenant_id));
        if let Some(class) = class {
            condition = condition.add(price_overlay::Column::ScopeClass.eq(class.as_str()));
        }
        // The keyset walk resumes strictly after the row the cursor names, **on
        // both columns of the order**. Resuming on the id alone against an `(id,
        // revision)` sort drops every remaining revision of the overlay a page
        // boundary falls inside. Spelled as a disjunction rather than a row-value
        // comparison for `group_membership_repo`'s reason: both engines take `(a,
        // b) > (c, d)`, sea-orm has no builder for it, and a raw fragment would
        // put a dialect-shaped string into a repository that holds none.
        if let Some((after_id, after_revision)) = after {
            let after_revision = i64::try_from(after_revision).map_err(|_| {
                RepoError::Db(
                    "list pricing_price_overlay: the cursor names a revision no row can hold"
                        .to_owned(),
                )
            })?;
            condition = condition.add(
                Condition::any()
                    .add(price_overlay::Column::PriceOverlayId.gt(after_id))
                    .add(
                        Condition::all()
                            .add(price_overlay::Column::PriceOverlayId.eq(after_id))
                            .add(price_overlay::Column::Revision.gt(after_revision)),
                    ),
            );
        }
        // **Ordered by the cursor's key, not by precedence.** A
        // `precedence ASC, id ASC, revision ASC` order cannot carry a keyset walk:
        // it resumes in the wrong place and the walk skips rows -- exactly what
        // D-125 forbids. The cursor is `(id, revision)`, the same pair this orders
        // by (`api::rest::cursor::encode_id_and_revision`, 24 bytes); a single
        // 16-byte id is one column short of the order and drops rows at a
        // boundary inside an overlay.
        //
        // Precedence ordering is not lost information: it is a property of each
        // row, still on the wire, and a caller that assembles a stack reads it
        // from there. What a caller cannot do any more is assume the *sequence*
        // it receives is precedence order -- across pages that was never true
        // for any keyset walk, and within one page it was an accident of the
        // seed rather than a promise.
        let rows = price_overlay::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(condition)
            .order_by(price_overlay::Column::PriceOverlayId, Order::Asc)
            .order_by(price_overlay::Column::Revision, Order::Asc)
            .limit(limit)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list pricing_price_overlay: {e}")))?;

        // **Two statements for the page, not two per row.** `read_lines` is itself
        // two, so hydrating a page one row at a time cost `1 + 2*limit` round
        // trips — the fan-out `read_lines`' own doc names and leaves to its
        // callers. `price_repo::hydrate_bands` batches the identical parent/child
        // shape the same way.
        let mut lines_by_row = read_lines_for_page(&conn, scope, tenant_id, &rows).await?;
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let lines = lines_by_row
                .remove(&(row.price_overlay_id, row.revision))
                .unwrap_or_default();
            records.push(record_of(&row, lines)?);
        }
        Ok(records)
    }

    /// One `OData` page of overlay revisions. Default order is
    /// `price_overlay_id asc` with `revision` as the seekset tiebreaker.
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
    /// malformed `$filter` / `$orderby` / cursor.
    pub async fn list_odata(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<OverlayRecord>, OdataPageError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| OdataPageError::Db(format!("pricing_price_overlay conn: {e}")))?;
        let base_select = price_overlay::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(price_overlay::Column::TenantId.eq(tenant_id)));
        let query = query_with_default_order(query, &[OverlayFilterField::PriceOverlayId]);
        // `revision` is the tiebreaker below and only the second half of the
        // `(price_overlay_id, revision)` key, so a caller's `$orderby` would
        // leave `[field, revision]` — two overlays both `published` at revision
        // 1 then share a keyset value and one is dropped at the page boundary.
        let query = query_with_unique_order(&query, &[OverlayFilterField::PriceOverlayId]);
        let page = paginate_odata::<
            OverlayFilterField,
            OverlayODataMapper,
            price_overlay::Entity,
            price_overlay::Model,
            _,
            _,
        >(
            base_select,
            &conn,
            &query,
            ("revision", SortDir::Asc),
            LIST_LIMIT_CFG,
            |m| m,
        )
        .await
        .map_err(map_odata_err)?;

        let mut lines_by_row = read_lines_for_page(&conn, scope, tenant_id, &page.items)
            .await
            .map_err(OdataPageError::Repo)?;
        let mut items = Vec::with_capacity(page.items.len());
        for row in page.items {
            let lines = lines_by_row
                .remove(&(row.price_overlay_id, row.revision))
                .unwrap_or_default();
            items.push(record_of(&row, lines).map_err(OdataPageError::Repo)?);
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// Is `value` declared in the taxonomy `class` validates against, and active?
    ///
    /// `inst-plv-scope`'s lookup. The classless scope consults nothing and
    /// answers `true`: it has no value, so there is nothing to be undeclared.
    ///
    /// **`customer_group` reads `pricing_customer_group_taxonomy`, and used to
    /// answer `false` unconditionally.** D-223 chose that refusal *while the
    /// class had no value universe* — "reversible in one line when the
    /// membership half lands" are its own words, and the table landed:
    /// `pricing_customer_group_taxonomy` carries the four Slice 4 taxonomies' shape,
    /// `TaxonomyRepo::replace_customer_groups` writes it, and
    /// `api::rest::customer_groups` mounts the `GET`/`PUT` pair §5 gives it. That
    /// table's migration doc files this wiring as the follow-up it *"only gives
    /// the class somewhere to read"* for. Left as it was, the arm refused a
    /// `customerGroup` overlay with `SCOPE_VALUE_UNKNOWN` against a universe that
    /// exists — which is not the fail-closed reading of anything, only a stale
    /// one.
    ///
    /// The refusal it *is* still fail-closed about is unchanged: an undeclared
    /// value, and a `retired` one, both answer `false` here exactly as on the
    /// four siblings.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn taxonomy_declares(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        selector: &ScopeSelector,
    ) -> Result<bool, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("taxonomy conn: {e}")))?;
        declares_selector(&conn, scope, tenant_id, selector).await
    }
}

// ---------------------------------------------------------------------------
// Taxonomy lookup — one function per table, because each is a distinct entity.
// ---------------------------------------------------------------------------

/// The `active` predicate every one of the five shares.
///
/// A **retired** value does not declare anything: §6 guards retirement while a
/// value is referenced, and a value that reached `retired` anyway must not
/// validate a new overlay against itself.
async fn declares(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: ScopeClass,
    value: &ScopeValue,
) -> Result<bool, RepoError> {
    // The taxonomy vocabulary's own renderer, not a second spelling of it. This
    // half of the class fails loud rather than open — a token that stopped
    // matching empties the value universe and refuses every publish — but it is
    // the same defect, and the fix is to have one renderer rather than to rely on
    // the direction the second one happens to break in.
    let active = TaxonomyState::Active.as_str();
    let found = match class {
        ScopeClass::Global => return Ok(true),
        ScopeClass::Region => region_taxonomy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(region_taxonomy::Column::TenantId.eq(tenant_id))
                    .add(region_taxonomy::Column::Value.eq(value.as_str()))
                    .add(region_taxonomy::Column::State.eq(active)),
            )
            .one(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_region_taxonomy: {e}")))?
            .is_some(),
        ScopeClass::Brand => brand_taxonomy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(brand_taxonomy::Column::TenantId.eq(tenant_id))
                    .add(brand_taxonomy::Column::Value.eq(value.as_str()))
                    .add(brand_taxonomy::Column::State.eq(active)),
            )
            .one(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_brand_taxonomy: {e}")))?
            .is_some(),
        ScopeClass::Partner => partner_taxonomy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(partner_taxonomy::Column::TenantId.eq(tenant_id))
                    .add(partner_taxonomy::Column::Value.eq(value.as_str()))
                    .add(partner_taxonomy::Column::State.eq(active)),
            )
            .one(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_partner_taxonomy: {e}")))?
            .is_some(),
        ScopeClass::OrgTier => org_tier_taxonomy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(org_tier_taxonomy::Column::TenantId.eq(tenant_id))
                    .add(org_tier_taxonomy::Column::Value.eq(value.as_str()))
                    .add(org_tier_taxonomy::Column::State.eq(active)),
            )
            .one(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_org_tier_taxonomy: {e}")))?
            .is_some(),
        // The fifth table, read exactly as its four siblings are. It is a
        // separate arm rather than a shared one because each class is a distinct
        // entity with its own `Column` type; see this section's heading.
        ScopeClass::CustomerGroup => customer_group_taxonomy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(customer_group_taxonomy::Column::TenantId.eq(tenant_id))
                    .add(customer_group_taxonomy::Column::Value.eq(value.as_str()))
                    .add(customer_group_taxonomy::Column::State.eq(active)),
            )
            .one(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_customer_group_taxonomy: {e}")))?
            .is_some(),
    };
    Ok(found)
}

// ---------------------------------------------------------------------------
// Statements.
// ---------------------------------------------------------------------------

/// Append this overlay mutation's audit record — D-14, `inst-rb-audit`.
///
/// Called **inside** each mutation's own transaction, which is the whole of what
/// D-14 asks for: a record that commits with its mutation cannot be lost by a crash
/// between the two, and a failure to write it rolls the mutation back rather than
/// leaving a trail that is silently incomplete. `plan_repo::record_revision_mutation`
/// is the same arrangement one plane over.
///
/// The chain is [`audit_repo::overlay_chain`] and **not** the target plan's, for the
/// reason D-135 gives: a segment is keyed on the audited subject's *aggregate*, and
/// S5 §6 lists an overlay as one in its own right. An overlay's lines may target
/// several plans, or none — `LineKey::list_default` — so there is not even a
/// well-defined plan to borrow.
///
/// `before_state` is `None` throughout, and that is a statement rather than a
/// shortcut: three of the four mutations create a row (the overlay, its successor
/// revision, its replacement line set) and have no before, and `publish_revision`'s
/// predecessor state is derivable from the record one `seq` back on the same chain.
/// Recording a before that the writer had to re-read would be a second answer to a
/// question the chain already answers.
///
/// # The subject is [`audit_repo::overlay_revision_ref`], never a bare id
///
/// Z8-6. That helper's own doc calls itself *"one overlay revision's durable **audit
/// and approval** name"*, and it reached one of the two stores: `infra::approval`
/// and `infra::overlay_publish` call it, and this writer — the only one on the audit
/// side — hand-wrote `price_overlay_id.to_string()`. So the record of an act and the
/// unit authorizing it named one subject two different ways, and `subject_ref` is
/// the only join between the planes: a walk over it found nothing. The revision was
/// not *lost* — it rides in `after_state` — but a value in a payload is not a name,
/// and only the name is joinable.
///
/// The correction is `window_ref`'s, applied rather than reinvented: that helper
/// records the same divergence for windows and states the rule — *"it keeps both
/// stores on one spelling: the audit record and the approval record of one act name
/// it identically, which is the alignment D-158 is about, and it is why the helper
/// moved rather than its three call sites."* Here too the call sites do not move;
/// they already pass the revision they acted on, which is why each of the four
/// records now names its own rather than sharing one.
///
/// **Not vocabulary.** What the design set declares is the `subject_kind` token
/// (S5 §6), and `AuditSubjectKind::Overlay` is unchanged — no roster, `CHECK` or
/// enum moves with this. How a subject of that kind is *spelled* is the store's
/// own business, and `subject_ref` is free text on both backends.
async fn record_overlay_mutation(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_overlay_id: Uuid,
    revision: i64,
    action: AuditAction,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    // Checked, not cast. The column is `chk_pricing_price_overlay_revision CHECK
    // (revision >= 0)` (`pricing_price_overlay`), so a negative here is a row the store
    // should not hold rather than a request anyone can be refused for — which is
    // what `CorruptRow` is for. A saturating or wrapping conversion would put a
    // *plausible* revision into a durable name, and the two stores would agree on
    // a name that denotes the wrong revision, which is worse than disagreeing.
    let revision_named = u64::try_from(revision).map_err(|_| {
        RepoError::CorruptRow(format!(
            "pricing_price_overlay {price_overlay_id} carries revision {revision}, which \
             chk_pricing_price_overlay_revision forbids and no audit name can denote"
        ))
    })?;
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::overlay_chain(price_overlay_id),
            recorded_at: stamp.recorded_at,
            actor_principal_id: stamp.actor_principal_id,
            action,
            subject_kind: AuditSubjectKind::Overlay,
            subject_ref: audit_repo::overlay_revision_ref(price_overlay_id, revision_named),
            before_state: None,
            after_state: Some(serde_json::json!({ "revision": revision })),
            // **No approval to name**, and not because the unit is unwired —
            // D-225's `ApprovalService::submit_overlay_on` wires it. The reason is
            // narrower: none of the **four mutations this function records** runs
            // under a unit. The submit is a fifth act, recorded by the approval plane on
            // this same segment, and *its* record does carry an `approval_ref` —
            // which is why an auditor is not missing the link, only this end of it.
            // The publish is the one act that follows an approved unit, and
            // threading the id would mean carrying it through
            // `publish_revision_on`'s whole signature; left undone deliberately
            // rather than by omission, and it is a change to that path, not to
            // this one.
            approval_ref: None,
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map(|_| ())
}

/// [`OverlayRepo::create`]'s body on a runner the caller owns.
///
/// The method opens its own transaction; this form runs on the caller's, which is
/// what the at-most-once gate needs. `infra::idempotent::guarded` claims the
/// client key and runs the guarded mutation **in one transaction** — split across
/// two the guarantee inverts, and `POST /price-overlays` is the route that needed
/// it: it demanded an `Idempotency-Key` and discarded it, and with no uniqueness
/// index standing behind the create a retry authored a *second draft overlay*
/// rather than being answered the first one.
///
/// **The runner must be a transaction**, `bundle_repo::replace_composition_on`'s
/// house rule and for the same reason doubled: this writes the header, the whole
/// line set *and* the D-14 audit record, so on a bare connection they would be
/// separate autocommit statements and a failure part way would leave a committed
/// overlay with no lines and no record of anyone making it. Both current callers
/// supply one — [`OverlayRepo::create`]'s own, and the gate's.
///
/// # Errors
/// Exactly [`OverlayRepo::create`]'s.
pub async fn create_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    new: NewOverlay,
    lines: Vec<OverlayLine>,
    stamp: AuditStamp,
) -> Result<u64, RepoError> {
    // The overlay's own interval is an authored, published, **compared** instant
    // pair — `OverlayInterval::intersects` is what `check_dating` decides
    // collisions with — so it meets D-144's quantum here, exactly as
    // `window_repo::schedule` gates the identically-shaped pair one module over.
    // Restated on this path rather than left to the method: a caller reaching the
    // create through its runner-taking form would otherwise write an instant the
    // direct path refuses, and the two must not disagree about what a valid
    // request is. `OverlayRepo::create` keeps its copy **before** opening the
    // transaction, where a fault that needs no row read costs no transaction.
    check_authored_instant("effectiveFrom", new.interval.from)?;
    check_authored_instant("effectiveTo", new.interval.to)?;
    let row = price_overlay::ActiveModel {
        price_overlay_id: Set(new.price_overlay_id),
        revision: Set(0),
        tenant_id: Set(new.tenant_id),
        lifecycle_state: Set(OverlayLifecycle::Draft.as_str().to_owned()),
        scope_class: Set(new.scope.class().as_str().to_owned()),
        scope_value: Set(new.scope.stored_value().to_owned()),
        precedence: Set(new.precedence),
        effective_from: Set(new.interval.from),
        effective_to: Set(new.interval.to),
        tax_basis: Set(new.tax_basis.as_str().to_owned()),
        disclosure: Set(new.disclosure.as_str().to_owned()),
        target_ref: Set(render_target_ref(&new.target_ref)),
        row_version: Set(0),
    };
    insert_overlay(runner, scope, row).await?;
    write_lines(
        runner,
        scope,
        new.price_overlay_id,
        new.tenant_id,
        0,
        &lines,
    )
    .await?;
    record_overlay_mutation(
        runner,
        scope,
        new.tenant_id,
        new.price_overlay_id,
        0,
        AuditAction::Create,
        stamp,
    )
    .await?;
    Ok(0)
}

/// The **published** revision of one overlay, on a **runner**.
///
/// [`OverlayRepo::current`]'s body, lifted for [`load_on`]'s reason. D-234's
/// publish commit is the caller, and it needs this **before** its own flip: after
/// it, the predecessor stands `superseded` and the index shard it is leaving
/// (D-133's "two when a revision moves the scope value") would have to be
/// reconstructed from a state that has already moved.
///
/// # Errors
/// As [`OverlayRepo::load`].
pub(crate) async fn current_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_overlay_id: Uuid,
) -> Result<Option<OverlayRecord>, RepoError> {
    let Some(row) = revision_in_state(
        runner,
        scope,
        tenant_id,
        price_overlay_id,
        OverlayLifecycle::Published,
    )
    .await?
    else {
        return Ok(None);
    };
    let lines = read_lines(runner, scope, price_overlay_id, tenant_id, row.revision).await?;
    record_of(&row, lines).map(Some)
}

/// Publish a draft revision and supersede its predecessor, on a **runner**.
///
/// [`OverlayRepo::publish_revision`]'s body, lifted for the same reason
/// [`load_on`] was: a caller already inside a transaction needs it. D-234's
/// overlay publish commit is that caller, and for it the lifting is not a
/// convenience — the flip has to share one transaction with the registry handle,
/// the `pricing_catalog_version_ref` rows and the audit record, or a crash
/// between them leaves a published overlay no version ever projects.
///
/// # Errors
/// As [`OverlayRepo::publish_revision`].
pub(crate) async fn publish_revision_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_overlay_id: Uuid,
    revision: u64,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    let Ok(number) = i64::try_from(revision) else {
        return Err(RepoError::NotFound {
            subject: "price overlay revision".to_owned(),
            id: format!("{price_overlay_id}/{revision}"),
        });
    };
    // **The target is proved publishable before anything is
    // written.** Without this read the predecessor's demotion is
    // issued first and only then does the target turn out not to
    // be a draft — a write issued on the way to saying no. The
    // transaction rolls it back, so it was never a live defect; it
    // is the ordering `PlanRepo::publish` explicitly rejects for
    // this same operation, and matching it costs one read.
    if revision_in_state(
        runner,
        scope,
        tenant_id,
        price_overlay_id,
        OverlayLifecycle::Draft,
    )
    .await?
    .is_none_or(|draft| draft.revision != number)
    {
        return Err(RepoError::NotFound {
            subject: "open draft price overlay revision".to_owned(),
            id: format!("{price_overlay_id}/{revision}"),
        });
    }

    // The predecessor next: publishing before superseding would
    // put two published revisions on one `(class, precedence)`
    // for the length of one statement, which the partial index
    // refuses outright — and the predecessor is found **by
    // state**, so a lookup after the flip finds the row just
    // published.
    if let Some(predecessor) = revision_in_state(
        runner,
        scope,
        tenant_id,
        price_overlay_id,
        OverlayLifecycle::Published,
    )
    .await?
    {
        flip(
            runner,
            scope,
            tenant_id,
            price_overlay_id,
            predecessor.revision,
            OverlayLifecycle::Superseded,
        )
        .await?;
    }
    let moved = flip(
        runner,
        scope,
        tenant_id,
        price_overlay_id,
        number,
        OverlayLifecycle::Published,
    )
    .await?;
    if moved == 0 {
        return Err(RepoError::NotFound {
            subject: "open draft price overlay revision".to_owned(),
            id: format!("{price_overlay_id}/{revision}"),
        });
    }
    // **One record for the pair**, not two. The supersession is not
    // a second act: §6 requires the flip and the publish to be one
    // commit, so a trail carrying them separately would show a state
    // the store cannot be in.
    record_overlay_mutation(
        runner,
        scope,
        tenant_id,
        price_overlay_id,
        number,
        AuditAction::Publish,
        stamp,
    )
    .await?;
    Ok(())
}

/// One overlay revision, read on a **runner** rather than a provider.
///
/// [`OverlayRepo::load`]'s body, lifted so a caller already inside a transaction can
/// use it — `infra::approval::re_derive` is that caller, and it must read the subject
/// in the same transaction as the decision that compares it against the pin.
pub(crate) async fn load_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_overlay_id: Uuid,
    revision: u64,
) -> Result<Option<OverlayRecord>, RepoError> {
    let Ok(number) = i64::try_from(revision) else {
        return Ok(None);
    };
    let Some(row) = price_overlay::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_overlay::Column::TenantId.eq(tenant_id))
                .add(price_overlay::Column::PriceOverlayId.eq(price_overlay_id))
                .add(price_overlay::Column::Revision.eq(number)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_price_overlay: {e}")))?
    else {
        return Ok(None);
    };
    let lines = read_lines(runner, scope, price_overlay_id, tenant_id, number).await?;
    record_of(&row, lines).map(Some)
}

async fn insert_overlay(
    runner: &impl DBRunner,
    scope: &AccessScope,
    row: price_overlay::ActiveModel,
) -> Result<(), RepoError> {
    price_overlay::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(|e| RepoError::Db(format!("pricing_price_overlay scope: {e}")))?
        .exec(runner)
        .await
        // **No precedence mapping here, deliberately.** Both callers insert a
        // `draft`, and the precedence index is partial on `published`, so it
        // cannot fire on this statement — an arm here would be dead code.
        // The live producer of `OverlayPrecedenceHeld` is `flip`, which is where a
        // draft becomes the published row that claims the slot.
        .map_err(|e| RepoError::Db(format!("insert pricing_price_overlay: {e}")))
        .map(|_| ())
}

/// One revision of one overlay in a given state, if it has one.
async fn revision_in_state(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_overlay_id: Uuid,
    state: OverlayLifecycle,
) -> Result<Option<price_overlay::Model>, RepoError> {
    price_overlay::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_overlay::Column::TenantId.eq(tenant_id))
                .add(price_overlay::Column::PriceOverlayId.eq(price_overlay_id))
                .add(price_overlay::Column::LifecycleState.eq(state.as_str())),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_price_overlay by state: {e}")))
}

/// Move one revision's `lifecycle_state`, answering how many rows it moved.
async fn flip(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_overlay_id: Uuid,
    revision: i64,
    to: OverlayLifecycle,
) -> Result<u64, RepoError> {
    let from = match to {
        OverlayLifecycle::Published | OverlayLifecycle::Abandoned => OverlayLifecycle::Draft,
        OverlayLifecycle::Superseded => OverlayLifecycle::Published,
        // A flip *to* draft is not a sanctioned edge; the trigger refuses it and
        // no caller here asks for one.
        OverlayLifecycle::Draft => return Ok(0),
    };
    price_overlay::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            price_overlay::Column::LifecycleState,
            Expr::value(to.as_str()),
        )
        .filter(
            Condition::all()
                .add(price_overlay::Column::TenantId.eq(tenant_id))
                .add(price_overlay::Column::PriceOverlayId.eq(price_overlay_id))
                .add(price_overlay::Column::Revision.eq(revision))
                // The source state is in the `WHERE`, so a second publisher's
                // flip matches zero rows rather than re-publishing.
                .add(price_overlay::Column::LifecycleState.eq(from.as_str())),
        )
        .exec(runner)
        .await
        .map(|r| r.rows_affected)
        .map_err(|e| precedence_held_or_db(&e, "flip pricing_price_overlay"))
}

/// The precedence slot's refusal, or an ordinary storage failure.
///
/// **The typed class first, the message only to say *which* index.**
/// [`crate::infra::storage::policy_guard_or_contention`] is the shape being
/// matched and its doc carries the argument for why message matching is the
/// narrow case at all. Without the conjunct this read text alone, so any failure
/// whose message happened to carry the DDL name — a lock report naming the index,
/// a planner error quoting it — was answered as
/// [`RepoError::OverlayPrecedenceHeld`], a refusal telling the caller to pick
/// another precedence for a fault that had nothing to do with one.
///
/// The fallback runs the safe way: an unrecognised message is
/// [`RepoError::Db`], which is today's answer for everything this does not
/// name.
fn precedence_held_or_db(err: &ScopeError, context: &str) -> RepoError {
    if err.is_unique_violation() && names_the_precedence_slot(&err.to_string()) {
        return RepoError::OverlayPrecedenceHeld;
    }
    RepoError::Db(format!("{context}: {err}"))
}

/// Does this driver message name `uq_pricing_price_overlay_precedence` rather
/// than any other index on the overlay table?
///
/// Two renderings, one per backend, both of them names `pricing_price_overlay` writes:
/// Postgres names the **index**, `SQLite` names the indexed **columns**. Neither
/// engine produces the other's spelling, so both arms are live.
///
/// Split out and taking a `&str` so both renderings are asserted directly,
/// without staging a database race for either — `names_the_policy_guard`'s
/// arrangement, for the same reason.
fn names_the_precedence_slot(message: &str) -> bool {
    message.contains("uq_pricing_price_overlay_precedence")
        || message.contains("pricing_price_overlay.precedence")
}

/// A stored `i64` counter as the `u64` a caller addresses it by.
///
/// Checked rather than `unwrap_or_default`, for the reason
/// [`record_overlay_mutation`] gives about its own conversion: a saturating or
/// defaulting conversion puts a *plausible* number where the true one cannot go,
/// and `0` is the most plausible of all — a caller told its overlay stands at
/// revision 0, or that it submitted row version 0, is reading a value that
/// denotes a different row. A stored counter outside `u64` is a row this gear
/// should not hold, which is what `CorruptRow` is for.
fn stored_counter(price_overlay_id: Uuid, column: &str, value: i64) -> Result<u64, RepoError> {
    u64::try_from(value).map_err(|_| {
        RepoError::CorruptRow(format!(
            "pricing_price_overlay {price_overlay_id} carries {column} {value}, which no caller \
             can address"
        ))
    })
}

/// Which of the three refusals a failed compare-and-swap owes the caller.
///
/// Not the guard — the swap is — so a concurrent publish landing between them
/// only changes which sentence the caller reads, never whether the write
/// happened.
async fn refuse_edit(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_overlay_id: Uuid,
    revision: i64,
    expected: i64,
) -> RepoError {
    let found = price_overlay::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_overlay::Column::TenantId.eq(tenant_id))
                .add(price_overlay::Column::PriceOverlayId.eq(price_overlay_id))
                .add(price_overlay::Column::Revision.eq(revision)),
        )
        .one(runner)
        .await;
    let subject = "price overlay revision".to_owned();
    let id = format!("{price_overlay_id}/{revision}");
    match found {
        Err(e) => RepoError::Db(format!("resolve pricing_price_overlay refusal: {e}")),
        Ok(None) => RepoError::NotFound { subject, id },
        Ok(Some(row)) if row.lifecycle_state != OverlayLifecycle::Draft.as_str() => {
            RepoError::NotDraft {
                subject,
                id,
                state: row.lifecycle_state,
            }
        }
        Ok(Some(row)) => match (
            stored_counter(price_overlay_id, "row_version", row.row_version),
            stored_counter(price_overlay_id, "the submitted row_version", expected),
        ) {
            (Ok(current), Ok(submitted)) => RepoError::StaleRowVersion {
                subject,
                id,
                current,
                submitted,
            },
            (Err(corrupt), _) | (_, Err(corrupt)) => corrupt,
        },
    }
}

/// Write one revision's whole line set — lines before amounts, per the foreign
/// key.
///
/// **The line key's `cohort` meets D-144's quantum here** (`super::check_authored_instant`),
/// beside the nil-plan sentinel and for the same reason: this is the one funnel
/// every line write passes through — `create`, `replace_lines` and the
/// `open_revision` copy — so a gate on any single caller would leave a plane open.
///
/// It is the axis the price plane already gates twice (`ScopeKey::new` and the
/// cutover narrowing), and the two planes are matched against each other for
/// **equality**: [`published_generation`] rebuilds the price plane's milliseconds,
/// so an unquantized cohort belongs to no generation set. Without this the fault
/// still surfaced — fail-closed, as `OVERLAY_LINE_COHORT_UNKNOWN` — but naming the
/// wrong cause: the operator was told the plan publishes no generation at that
/// instant when what was wrong was the instant's precision, on exactly the value a
/// client gets by copying a `timestamptz` rendering out of another gear. Two rules
/// behind that single gate were disarmed as well: `check_dating` skips a line whose
/// key does not match a holder's, and `check_duplicate_keys` counts two cohorts a
/// microsecond apart as two keys.
///
/// **The read path is deliberately not gated.** `line_of` rebuilds a stored key,
/// and refusing there would make a row already written unreadable rather than
/// unwritable — the authoring edge is where an author can still correct one value.
async fn write_lines(
    runner: &impl DBRunner,
    scope: &AccessScope,
    price_overlay_id: Uuid,
    tenant_id: Uuid,
    revision: i64,
    lines: &[OverlayLine],
) -> Result<(), RepoError> {
    for line in lines {
        check_authored_instant("cohort", line.key.cohort())?;
        if line.key.plan_id() == Some(PlanId::new(Uuid::nil())) {
            // The line key's index coalesces an absent plan to the nil uuid, so
            // a request naming it would key as the list-default line and collide
            // with it. The sentinel is the store's; this is what keeps it
            // unforgeable. See `pricing_price_overlay_line`'s migration doc.
            return Err(RepoError::ValueOutOfRange {
                field: "plan_id".to_owned(),
                value: Uuid::nil().to_string(),
            });
        }
        let row = price_overlay_line::ActiveModel {
            line_id: Set(line.line_id),
            overlay_revision: Set(revision),
            price_overlay_id: Set(price_overlay_id),
            tenant_id: Set(tenant_id),
            plan_id: Set(line.key.plan_id().map(PlanId::get)),
            target_sku: Set(line.key.target_sku().map(|s| s.as_str().to_owned())),
            cohort: Set(line.key.cohort()),
            adjustment_kind: Set(line.adjustment.kind().as_str().to_owned()),
            magnitude_kind: Set(line.adjustment.magnitude_kind().as_str().to_owned()),
            adjustment_value: Set(line.adjustment.percent_bp()),
        };
        price_overlay_line::Entity::insert(row.clone())
            .secure()
            .scope_with_model(scope, &row)
            .map_err(|e| RepoError::Db(format!("pricing_price_overlay_line scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| {
                // `line_id` is caller-mintable and the line's primary key is
                // `(tenant_id, overlay_revision, line_id)` — **not** scoped by
                // overlay — so pasting a line id read off one of this tenant's
                // overlays into another at the same revision is a primary-key
                // collision. That is a well-formed request whose whole remedy is to
                // omit one field, so it is a refusal the caller can act on rather
                // than a storage fault.
                //
                // `tenant_id` joined the key under `pricing_price_overlay_line`. Before it,
                // the same collision spanned **every** tenant, so this refusal was
                // also an oracle over another tenant's line ids; the sentence it
                // renders is unchanged because what the author has to do about it
                // is.
                if is_line_identity_collision(&e) {
                    RepoError::ValueOutOfRange {
                        field: "line_id".to_owned(),
                        value: line.line_id.to_string(),
                    }
                } else {
                    RepoError::Db(format!("insert pricing_price_overlay_line: {e}"))
                }
            })?;

        let Some(amounts) = line.adjustment.amounts() else {
            continue;
        };
        for (currency, value) in amounts.iter() {
            let row = price_overlay_line_amount::ActiveModel {
                line_id: Set(line.line_id),
                overlay_revision: Set(revision),
                currency: Set(currency.as_str().to_owned()),
                tenant_id: Set(tenant_id),
                value_minor: Set(value),
            };
            price_overlay_line_amount::Entity::insert(row.clone())
                .secure()
                .scope_with_model(scope, &row)
                .map_err(|e| {
                    RepoError::Db(format!("pricing_price_overlay_line_amount scope: {e}"))
                })?
                .exec(runner)
                .await
                // **Untyped, and unreachable — review A1-4, recorded rather than
                // pre-emptively classified.**
                //
                // Two independent guards keep every uniqueness failure of this
                // table out of reach, and each is worth naming because the
                // catch-all arms the moment either moves. (a) A duplicate currency
                // within one line cannot arrive: `AmountSet` is a
                // `BTreeMap<CurrencyCode, i64>` whose constructor documents
                // last-wins, so the iteration above yields each currency once.
                // (b) A cross-line collision cannot arrive: the line insert thirty
                // lines up refuses the colliding `line_id` first, and a `line_id`
                // past that check is unique at that revision **within the tenant**,
                // so its amounts are too.
                //
                // Guard (b) is the one `pricing_price_overlay_line` had to keep true. That
                // migration made two tenants able to hold one
                // `(line_id, overlay_revision)`, which would have armed this
                // catch-all as a `500` had the amount key not widened with the
                // line's — so it widened in the same statement list, to
                // `(tenant_id, overlay_revision, line_id, currency)`. A typed code
                // here today would be a code nothing can produce and no document
                // declares.
                .map_err(|e| {
                    RepoError::Db(format!("insert pricing_price_overlay_line_amount: {e}"))
                })?;
        }
    }
    Ok(())
}

/// Drop one revision's lines — amounts before lines, per the foreign key.
async fn drop_lines(
    runner: &impl DBRunner,
    scope: &AccessScope,
    price_overlay_id: Uuid,
    tenant_id: Uuid,
    revision: i64,
) -> Result<(), RepoError> {
    let doomed = read_line_rows(runner, scope, price_overlay_id, tenant_id, revision).await?;
    // One statement for the whole set, not one per line. The batched predicate is
    // the same one `read_amounts_by_line` builds a hundred lines down; the order
    // is unchanged — amounts before lines, so no amount is ever orphaned.
    if !doomed.is_empty() {
        let line_ids: Vec<Uuid> = doomed.iter().map(|line| line.line_id).collect();
        price_overlay_line_amount::Entity::delete_many()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(price_overlay_line_amount::Column::TenantId.eq(tenant_id))
                    .add(price_overlay_line_amount::Column::LineId.is_in(line_ids))
                    .add(price_overlay_line_amount::Column::OverlayRevision.eq(revision)),
            )
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("clear overlay line amounts: {e}")))?;
    }
    price_overlay_line::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(line_revision(price_overlay_id, tenant_id, revision))
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("clear overlay lines: {e}")))?;
    Ok(())
}

/// Copy one revision's whole line set onto `to`, **preserving `line_id`**.
async fn copy_lines(
    runner: &impl DBRunner,
    scope: &AccessScope,
    price_overlay_id: Uuid,
    tenant_id: Uuid,
    from: i64,
    to: i64,
) -> Result<(), RepoError> {
    let lines = read_lines(runner, scope, price_overlay_id, tenant_id, from).await?;
    write_lines(runner, scope, price_overlay_id, tenant_id, to, &lines).await
}

/// *This overlay, this tenant, this revision* — the predicate the line table is
/// ranged over by.
fn line_revision(price_overlay_id: Uuid, tenant_id: Uuid, revision: i64) -> Condition {
    Condition::all()
        .add(price_overlay_line::Column::TenantId.eq(tenant_id))
        .add(price_overlay_line::Column::PriceOverlayId.eq(price_overlay_id))
        .add(price_overlay_line::Column::OverlayRevision.eq(revision))
}

async fn read_line_rows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    price_overlay_id: Uuid,
    tenant_id: Uuid,
    revision: i64,
) -> Result<Vec<price_overlay_line::Model>, RepoError> {
    price_overlay_line::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(line_revision(price_overlay_id, tenant_id, revision))
        .order_by(price_overlay_line::Column::LineId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read overlay lines: {e}")))
}

/// Read one revision's lines back into the domain's shape.
///
/// **Two statements, whatever the line count.** Reading the amounts per line
/// makes every caller of this function a fan-out: `list` pays it once per row of
/// a page, and `overlay_facts` — which runs inside the publish transaction and
/// over the tenant's *whole* published set — pays it once per line of every
/// published overlay. The batched read is
/// [`read_amounts_by_line`], and the property is counted rather than argued in
/// `tests/sqlite_overlay_world_fanout.rs`.
async fn read_lines(
    runner: &impl DBRunner,
    scope: &AccessScope,
    price_overlay_id: Uuid,
    tenant_id: Uuid,
    revision: i64,
) -> Result<Vec<OverlayLine>, RepoError> {
    let rows = read_line_rows(runner, scope, price_overlay_id, tenant_id, revision).await?;
    let mut amounts = read_amounts_by_line(runner, scope, tenant_id, revision, &rows).await?;
    let mut lines = Vec::with_capacity(rows.len());
    for row in rows {
        // A line with no amount rows is not an error: a `percent_bp` line has
        // none by construction, and `line_of` reads the empty set exactly as a
        // per-line query's empty answer would be read.
        let own = amounts.remove(&row.line_id).unwrap_or_default();
        lines.push(line_of(&row, &own)?);
    }
    Ok(lines)
}

/// Every line of a whole page, in two statements, grouped by `(overlay, revision)`.
///
/// The per-revision predicate widens to `is_in` over the page's ids and its
/// revisions, which over-selects across the cross product, so the grouping keys
/// on the **pair** and a row belonging to no requested pair is dropped. Ordering
/// within a revision is preserved by the `LineId` sort, which is what `line_of`'s
/// callers read.
///
/// The amount map keys on the pair for the same reason, and it is the sharper of
/// the two: `line_id` is carried forward verbatim when `open_revision` copies a
/// line, so one id names a row in every revision of its overlay and the amount
/// table's key is `(line_id, overlay_revision, currency, tenant_id)`. Keyed on
/// `line_id` alone this read serves one revision another's money and the next
/// revision none — a page holds both, since `list` walks `(id, revision)`.
async fn read_lines_for_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    page: &[price_overlay::Model],
) -> Result<BTreeMap<(Uuid, i64), Vec<OverlayLine>>, RepoError> {
    let mut out: BTreeMap<(Uuid, i64), Vec<OverlayLine>> = BTreeMap::new();
    if page.is_empty() {
        return Ok(out);
    }
    let wanted: BTreeSet<(Uuid, i64)> = page
        .iter()
        .map(|row| (row.price_overlay_id, row.revision))
        .collect();
    let ids: Vec<Uuid> = wanted.iter().map(|(id, _)| *id).collect();
    let revisions: Vec<i64> = wanted.iter().map(|(_, rev)| *rev).collect();

    let line_rows = price_overlay_line::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_overlay_line::Column::TenantId.eq(tenant_id))
                .add(price_overlay_line::Column::PriceOverlayId.is_in(ids))
                .add(price_overlay_line::Column::OverlayRevision.is_in(revisions)),
        )
        .order_by(price_overlay_line::Column::LineId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read overlay lines: {e}")))?;
    let mine: Vec<price_overlay_line::Model> = line_rows
        .into_iter()
        .filter(|row| wanted.contains(&(row.price_overlay_id, row.overlay_revision)))
        .collect();

    let line_ids: Vec<Uuid> = mine.iter().map(|row| row.line_id).collect();
    let line_revisions: Vec<i64> = mine
        .iter()
        .map(|row| row.overlay_revision)
        .collect::<BTreeSet<i64>>()
        .into_iter()
        .collect();
    let mut amounts: BTreeMap<(Uuid, i64), Vec<price_overlay_line_amount::Model>> = BTreeMap::new();
    if !line_ids.is_empty() {
        let amount_rows = price_overlay_line_amount::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(price_overlay_line_amount::Column::TenantId.eq(tenant_id))
                    .add(price_overlay_line_amount::Column::OverlayRevision.is_in(line_revisions))
                    .add(price_overlay_line_amount::Column::LineId.is_in(line_ids)),
            )
            .order_by(price_overlay_line_amount::Column::LineId, Order::Asc)
            .order_by(price_overlay_line_amount::Column::Currency, Order::Asc)
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read overlay line amounts: {e}")))?;
        for row in amount_rows {
            amounts
                .entry((row.line_id, row.overlay_revision))
                .or_default()
                .push(row);
        }
    }

    for row in mine {
        let own = amounts
            .remove(&(row.line_id, row.overlay_revision))
            .unwrap_or_default();
        let key = (row.price_overlay_id, row.overlay_revision);
        out.entry(key).or_default().push(line_of(&row, &own)?);
    }
    Ok(out)
}

/// Every amount row of these lines, in one statement, grouped by line.
///
/// The filter is the union of the per-line filters it replaces — same tenant,
/// same revision, `line_id` in the set — so it can neither reach another
/// revision's amounts nor another overlay's: `line_id` is unique per revision
/// across overlays, which is what `(line_id, overlay_revision)` being the line's
/// primary key means.
///
/// Ordered by `line_id` then `currency` so each group keeps the currency order the
/// per-line read produced; `AmountSet` is a `BTreeMap` and would re-sort anyway,
/// but a reader comparing the two shapes should not have to know that.
///
/// An empty line set issues **no** statement rather than a `WHERE line_id IN ()`.
async fn read_amounts_by_line(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    revision: i64,
    lines: &[price_overlay_line::Model],
) -> Result<BTreeMap<Uuid, Vec<price_overlay_line_amount::Model>>, RepoError> {
    let mut by_line: BTreeMap<Uuid, Vec<price_overlay_line_amount::Model>> = BTreeMap::new();
    if lines.is_empty() {
        return Ok(by_line);
    }
    let ids: Vec<Uuid> = lines.iter().map(|line| line.line_id).collect();
    let rows = price_overlay_line_amount::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_overlay_line_amount::Column::TenantId.eq(tenant_id))
                .add(price_overlay_line_amount::Column::OverlayRevision.eq(revision))
                .add(price_overlay_line_amount::Column::LineId.is_in(ids)),
        )
        .order_by(price_overlay_line_amount::Column::LineId, Order::Asc)
        .order_by(price_overlay_line_amount::Column::Currency, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read overlay line amounts: {e}")))?;
    for row in rows {
        by_line.entry(row.line_id).or_default().push(row);
    }
    Ok(by_line)
}

/// A stored line and its values, as the domain reads them.
///
/// Every token is parsed rather than carried as a string, so a value no `CHECK`
/// should have admitted is a [`RepoError::CorruptRow`] here instead of an
/// unmatched arm three layers up.
fn line_of(
    row: &price_overlay_line::Model,
    amounts: &[price_overlay_line_amount::Model],
) -> Result<OverlayLine, RepoError> {
    let corrupt = |what: &str| {
        RepoError::CorruptRow(format!(
            "pricing_price_overlay_line {} carries an unusable {what}",
            row.line_id
        ))
    };

    let key = match (row.plan_id, row.target_sku.as_deref(), row.cohort) {
        (None, None, None) => LineKey::list_default(),
        (Some(plan), None, None) => LineKey::for_plan(PlanId::new(plan)),
        (Some(plan), Some(sku), None) => LineKey::for_sku(
            PlanId::new(plan),
            TargetSku::new(sku).ok_or_else(|| corrupt("target_sku"))?,
        ),
        (Some(plan), None, Some(cohort)) => LineKey::for_plan(PlanId::new(plan))
            .for_cohort(cohort)
            .ok_or_else(|| corrupt("cohort"))?,
        (Some(plan), Some(sku), Some(cohort)) => LineKey::for_sku(
            PlanId::new(plan),
            TargetSku::new(sku).ok_or_else(|| corrupt("target_sku"))?,
        )
        .for_cohort(cohort)
        .ok_or_else(|| corrupt("cohort"))?,
        // A SKU or a cohort with no plan: refused by two `CHECK`s and
        // unconstructible in the domain, so reaching here means the table was
        // written around.
        (None, _, _) => return Err(corrupt("plan-less line key")),
    };

    let mut set = Vec::with_capacity(amounts.len());
    for amount in amounts {
        let currency = CurrencyCode::new(&amount.currency).map_err(|_| corrupt("currency"))?;
        set.push((currency, amount.value_minor));
    }
    let amount_set = AmountSet::new(set);

    // Both kind columns go through [`read_token`] against the domain's own `ALL`,
    // like the four tokens `record_of` parses thirteen lines below. Hand-written
    // literal matches here would be the only place in the repository layer parsing
    // a stored enum column against literals — while the **write** thirty lines up
    // goes through `Adjustment::kind()`. One concept, a producer in the domain and
    // an inverse here, free to disagree: a renamed token leaves the writer storing
    // the new spelling and every read of it answering `CorruptRow`, with green
    // tests. `read_token`'s own doc states the rule.
    let magnitude = match read_token(
        "magnitude_kind",
        &row.magnitude_kind,
        MagnitudeKind::ALL,
        MagnitudeKind::as_str,
    )
    .map_err(|_| corrupt("magnitude_kind"))?
    {
        MagnitudeKind::PercentBp => {
            Magnitude::PercentBp(row.adjustment_value.ok_or_else(|| {
                // The biconditional `CHECK` forbids it, so this is a written-around
                // table rather than a caller mistake.
                corrupt("percent_bp line with no adjustment_value")
            })?)
        }
        MagnitudeKind::Amount => Magnitude::Amount(amount_set.clone()),
    };
    let adjustment = match read_token(
        "adjustment_kind",
        &row.adjustment_kind,
        AdjustmentKind::ALL,
        AdjustmentKind::as_str,
    )
    .map_err(|_| corrupt("adjustment_kind"))?
    {
        AdjustmentKind::Markup => Adjustment::Markup(magnitude),
        AdjustmentKind::Discount => Adjustment::Discount(magnitude),
        AdjustmentKind::Fixed => Adjustment::Fixed(amount_set),
    };

    Ok(OverlayLine {
        line_id: row.line_id,
        key,
        adjustment,
    })
}

/// A stored overlay row as the domain reads it.
fn record_of(
    row: &price_overlay::Model,
    lines: Vec<OverlayLine>,
) -> Result<OverlayRecord, RepoError> {
    let corrupt = |what: &str| {
        RepoError::CorruptRow(format!(
            "pricing_price_overlay {}/{} carries an unusable {what}",
            row.price_overlay_id, row.revision
        ))
    };
    let class = ScopeClass::parse(&row.scope_class).ok_or_else(|| corrupt("scope_class"))?;
    let scope = if class == ScopeClass::Global {
        ScopeSelector::Global
    } else {
        let value = ScopeValue::new(&row.scope_value).ok_or_else(|| corrupt("scope_value"))?;
        ScopeSelector::scoped(class, value).ok_or_else(|| corrupt("scope pairing"))?
    };

    Ok(OverlayRecord {
        price_overlay_id: row.price_overlay_id,
        revision: u64::try_from(row.revision).map_err(|_| corrupt("revision"))?,
        lifecycle_state: OverlayLifecycle::parse(&row.lifecycle_state)
            .ok_or_else(|| corrupt("lifecycle_state"))?,
        scope,
        precedence: row.precedence,
        interval: OverlayInterval {
            from: row.effective_from,
            to: row.effective_to,
        },
        tax_basis: TaxBasis::parse(&row.tax_basis).ok_or_else(|| corrupt("tax_basis"))?,
        disclosure: Disclosure::parse(&row.disclosure).ok_or_else(|| corrupt("disclosure"))?,
        target_ref: parse_target_ref(row.price_overlay_id, row.revision, &row.target_ref)?,
        row_version: row.row_version,
        lines,
    })
}

/// `target_ref` as the column holds it: `{"plans": ["<uuid>",...]}`.
///
/// A document rather than a child table because it is a **reference set** and
/// not content: nothing joins to it, no rule ranges over it per row, and §6
/// types it `jsonb`.
fn render_target_ref(target_ref: &TargetRef) -> sea_orm::JsonValue {
    serde_json::json!({
        "plans": target_ref.plans.iter().map(|p| p.get().to_string()).collect::<Vec<_>>(),
    })
}

/// The inverse, and it refuses rather than tolerates.
///
/// Reading an unreadable document as *no targets* looks fail-closed — every
/// non-default line is then outside the scope, so the overlay cannot publish. It
/// is fail-open into a **rule**: the validator is handed an empty target set as a
/// fact and judges on it, so a malformed document is indistinguishable from an
/// overlay that deliberately targets nothing, and the two get different remedies.
/// `plan_repo::read_change_targets` refuses the identical JSON shape, and this
/// file's own `sold_currency` is the same split for the same class.
///
/// # Errors
/// [`RepoError::CorruptRow`] when `plans` is present and is not an array of uuid
/// strings. An absent `plans` member stays the empty set, which is a document
/// this gear writes.
fn parse_target_ref(
    price_overlay_id: Uuid,
    revision: i64,
    value: &sea_orm::JsonValue,
) -> Result<TargetRef, RepoError> {
    let corrupt = |what: &str| {
        RepoError::CorruptRow(format!(
            "pricing_price_overlay {price_overlay_id}/{revision}: target_ref {what}"
        ))
    };
    let Some(raw) = value.get("plans") else {
        return Ok(TargetRef::default());
    };
    let sea_orm::JsonValue::Array(items) = raw else {
        return Err(corrupt(&format!("holds {raw}, which is not an array")));
    };
    let plans = items
        .iter()
        .map(|item| {
            item.as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .map(PlanId::new)
                .ok_or_else(|| corrupt(&format!("holds {item}, which is not a planId")))
        })
        .collect::<Result<_, _>>()?;
    Ok(TargetRef { plans })
}

// ---------------------------------------------------------------------------
// The world `PriceOverlayValidator` is judged against.
// ---------------------------------------------------------------------------

impl OverlayRepo {
    /// Resolve every fact [`crate::domain::overlay_rules::validate`] needs.
    ///
    /// The rules are pure with respect to what they are handed (§4.2), because
    /// the same set runs twice — as a pre-check at submit and again inside the
    /// publish-commit transaction — and the world moves between the two runs. So
    /// the reads live here and the judgement lives there, and this function is
    /// the only place that decides *what* a rule gets to see.
    ///
    /// # Two facts have no source in this crate, and they are named rather than
    /// # guessed
    ///
    /// * **`published_skus`** is derived from `pricing_plan.sku_id` — the SKU a
    ///   published plan revision publishes under — rendered as a string, because
    ///   §6 types the line's `target_sku` as one. There is no SKU *registry* in
    ///   this repository, so "a SKU this plan publishes" is exactly "the id on
    ///   the plan row" and nothing richer. A line naming any other SKU is
    ///   refused `OVERLAY_LINE_TARGET_UNKNOWN`, which is the fail-closed
    ///   direction; when the registry lands, this is the read that widens.
    /// * **`layers_beneath`** is D-138's warning domain, and it is
    ///   computed under the reading D-138's own words state — *"the
    ///   lowest-precedence layer able to match its target"*, i.e. numerically
    ///   lower. §F.1 leaves the stack's **sort direction** undecided, and under
    ///   the other reading this set is the complement of itself. See
    ///   `domain::overlay_rules`' module doc; it is an `[H]` owed entry.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn world_for(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        candidate: &OverlayRecord,
    ) -> Result<OverlayWorld, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("overlay world conn: {e}")))?;
        world_on(&conn, scope, tenant_id, candidate).await
    }
}

/// [`OverlayRepo::world_for`]'s body, on a caller-supplied runner.
///
/// **The split is what let §4.2's second validation run be built** (D-234's
/// residue (1)). That residue was recorded as blocked because `world_for` "takes
/// a provider rather than a runner" — true of the method, and the obstacle turned
/// out to be only the wrapper: all four fact readers below have taken
/// `&impl DBRunner` since they were written, so the world was always assemblable
/// inside a transaction and nothing but this entry point said otherwise.
///
/// The commit needs it on the **transaction**, not on a fresh connection: a world
/// read outside the transaction that flips the revision could be read before a
/// concurrent publish it has to see, and the re-validation would then pass
/// against a world that no longer exists by the time the flip lands.
pub(crate) async fn world_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    candidate: &OverlayRecord,
) -> Result<OverlayWorld, RepoError> {
    let scope_value_declared =
        declares_selector(runner, scope, tenant_id, &candidate.scope).await?;
    let plans = plan_facts(runner, scope, tenant_id, &candidate.target_ref.plans).await?;
    let markets = price_facts(runner, scope, tenant_id, &candidate.target_ref.plans).await?;
    let overlays = overlay_facts(runner, scope, tenant_id, candidate).await?;

    Ok(OverlayWorld {
        scope_value_declared,
        published_plans: plans.published,
        published_skus: plans.skus,
        retired_plans: plans.retired,
        sold_currencies: markets.currencies,
        published_cohorts: markets.cohorts,
        precedence_holder: overlays.precedence_holder,
        interval_holders: overlays.interval_holders,
        layers_beneath: overlays.layers_beneath,
        cross_class_ties: overlays.cross_class_ties,
    })
}

/// What the **plan** plane says about an overlay's targets.
struct PlanFacts {
    published: BTreeSet<PlanId>,
    retired: BTreeSet<PlanId>,
    skus: BTreeMap<PlanId, BTreeSet<TargetSku>>,
}

/// Which targets are published, which are retired, and under which SKU.
async fn plan_facts(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    targets: &[PlanId],
) -> Result<PlanFacts, RepoError> {
    let mut facts = PlanFacts {
        published: BTreeSet::new(),
        retired: BTreeSet::new(),
        skus: BTreeMap::new(),
    };
    if targets.is_empty() {
        return Ok(facts);
    }
    // One read over the target set, not one per target. This runs inside the
    // overlay submit's world assembly, so an overlay naming a dozen plans paid a
    // dozen round trips for a predicate `is_in` expresses once.
    let ids: Vec<Uuid> = targets.iter().map(|p| p.get()).collect();
    let revisions = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.is_in(ids)),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_plan for overlay world: {e}")))?;
    for revision in revisions {
        let plan_id = PlanId::new(revision.plan_id);
        // **`published` and `retired` are BOTH "current"** (D-128, and
        // `uq_pricing_plan_current`'s own predicate
        // `lifecycle_state IN ('published','retired')`). Retirement flips the
        // one current row in place, so a retired plan has **no** published
        // revision left — and reading that as "not published" would make
        // `TARGET_UNPUBLISHED` block every overlay on it.
        //
        // That is exactly the rule D-31 forbids: *"Retirement of a targeted
        // plan does not block on overlays … the overlay goes
        // dangling-and-flagged … remediation = end or retarget the overlay"*.
        // Blocking would also block the remediation, since ending or
        // retargeting an overlay is itself a submit.
        //
        // Read through [`LifecycleState::is_current_revision`] rather than
        // hand-enumerated: a hand-written token list is a copy of that
        // predicate, D-128 has widened it once already, and a copy that does
        // not move with it starts answering `TARGET_UNPUBLISHED` for a state
        // the machine calls current.
        let state = read_token(
            "pricing_plan.lifecycle_state",
            &revision.lifecycle_state,
            LifecycleState::ALL,
            LifecycleState::as_str,
        )?;
        if !state.is_current_revision() {
            continue;
        }
        facts.published.insert(plan_id);
        if let Some(sku) = revision.sku_id
            && let Some(named) = TargetSku::new(&sku.to_string())
        {
            facts.skus.entry(plan_id).or_default().insert(named);
        }
        if state == LifecycleState::Retired {
            facts.retired.insert(plan_id);
        }
    }
    Ok(facts)
}

/// What the **price** plane says about an overlay's targets.
struct MarketFacts {
    currencies: BTreeMap<PlanId, BTreeSet<CurrencyCode>>,
    cohorts: BTreeMap<PlanId, BTreeSet<OffsetDateTime>>,
}

/// Which markets each target sells, and which grandfathered generations it has
/// published (D-78).
async fn price_facts(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    targets: &[PlanId],
) -> Result<MarketFacts, RepoError> {
    let mut facts = MarketFacts {
        currencies: BTreeMap::new(),
        cohorts: BTreeMap::new(),
    };
    if targets.is_empty() {
        return Ok(facts);
    }
    // One read over the target set, for `plan_facts`' reason one screen up.
    let ids: Vec<Uuid> = targets.iter().map(|p| p.get()).collect();
    let rows = price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::PlanId.is_in(ids))
                .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_price for overlay world: {e}")))?;
    for row in rows {
        let plan_id = PlanId::new(row.plan_id);
        let currency = sold_currency(row.price_id, &row.currency)?;
        facts
            .currencies
            .entry(plan_id)
            .or_default()
            .insert(currency);
        if row.price_eligibility == PriceEligibility::ExistingGrandfathered.as_str()
            && let Some(at) = published_generation(&row.cohort)
        {
            facts.cohorts.entry(plan_id).or_default().insert(at);
        }
    }
    Ok(facts)
}

/// One published row's market, as [`price_facts`] must read it.
///
/// **Refused, not skipped**. `price_facts` held this inline as
/// `if let Ok(currency) = CurrencyCode::new(&row.currency)`, and an unparseable
/// stored currency simply dropped that market out of [`MarketFacts::currencies`] —
/// which becomes `OverlayWorld::sold_currencies`, whose only reader is
/// `domain::overlay_rules::coverage_domain`, the sole input to
/// `ADJUSTMENT_CURRENCY_NOT_COVERED`. So the reader failed **open** into a money
/// rule: the market the row sells stopped being a market the overlay had to cover,
/// and nothing else in the crate reads that map to catch it.
///
/// Physically reachable: [`CurrencyCode::new`] demands three ASCII letters while
/// `pricing_price.currency` is only `varchar(3) NOT NULL` with no letter or length
/// `CHECK` — and `SQLite` does not enforce `varchar(3)` at all. It still takes a row
/// no validated writer produces, which is exactly what [`RepoError::CorruptRow`] is
/// for; [`line_of`], `record_of`, [`plan_facts`] (through `read_token`) and
/// `plan_shape_repo::to_period_floor_cap` all answer the identical parse failure
/// that way, this one function excepted.
///
/// Split out and taking the column rather than the `Model` so the refusal is
/// assertable without staging a store — `bundle_repo::names_the_bundle_plan_slot`'s
/// arrangement, for the same reason.
///
/// # Errors
/// [`RepoError::CorruptRow`] naming the row and the column.
fn sold_currency(price_id: Uuid, stored: &str) -> Result<CurrencyCode, RepoError> {
    CurrencyCode::new(stored).map_err(|e| {
        RepoError::CorruptRow(format!(
            "pricing_price {price_id} carries an unusable pricing_price.currency: {e}"
        ))
    })
}

/// What the **overlay** plane says about a candidate.
struct OverlayFacts {
    precedence_holder: Option<Uuid>,
    interval_holders: Vec<PublishedLineInterval>,
    layers_beneath: BTreeSet<PlanId>,
    cross_class_ties: Vec<CrossClassTie>,
}

/// The precedence slot, the collision domain and D-138's lower layers.
///
/// One read of the tenant's published overlays serves all three, because all
/// three range over the same set. **The candidate's own overlay is skipped** —
/// D-107: an overlay never collides with another revision of itself, on either
/// the precedence slot or the interval.
///
/// # This read is deliberately unpaged, and what that costs is stated
///
/// [`OverlayRepo::list`] one screen up takes a `limit` because D-125 requires a
/// keyset walk of an *authoring* list. This one takes none, and must not: all
/// three facts are **absence** claims over the whole published set — "no other
/// overlay holds this precedence", "no published line's interval overlaps this
/// one" — and a page bound would turn each of them into "none on the first page",
/// which is a rule that passes by paging rather than by being true. §4.2 runs the
/// set twice per publish, the second time inside the commit transaction, so the
/// tenant's **published-overlay count** is what decides how long that transaction
/// stays open.
///
/// **The intended bound is therefore on the count itself, not on this query.** It
/// is the same shape as the row caps `pricing_policy_object` carries per tenant,
/// and there is no such cap on published overlays today: nothing in the design set
/// bounds it, so nothing here can enforce one. What this function now guarantees
/// is that the cost is linear in that count and **not** in the tenant's total line
/// count — [`read_lines`] reads a revision's amounts in one statement rather than
/// one per line, so the transaction holds `1 + 2n` statements for `n` published
/// overlays. `tests/sqlite_overlay_world_fanout.rs` counts them.
async fn overlay_facts(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    candidate: &OverlayRecord,
) -> Result<OverlayFacts, RepoError> {
    let published = price_overlay::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_overlay::Column::TenantId.eq(tenant_id))
                .add(
                    price_overlay::Column::LifecycleState.eq(OverlayLifecycle::Published.as_str()),
                ),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read published overlays: {e}")))?;

    let mut facts = OverlayFacts {
        precedence_holder: None,
        interval_holders: Vec::new(),
        layers_beneath: BTreeSet::new(),
        cross_class_ties: Vec::new(),
    };
    for row in published {
        if row.price_overlay_id == candidate.price_overlay_id {
            continue;
        }
        let record = record_of(&row, Vec::new())?;
        if record.scope.class() == candidate.scope.class()
            && record.precedence == candidate.precedence
        {
            facts.precedence_holder = Some(record.price_overlay_id);
        }
        let lines =
            read_lines(runner, scope, row.price_overlay_id, tenant_id, row.revision).await?;
        for line in &lines {
            facts.interval_holders.push(PublishedLineInterval {
                price_overlay_id: record.price_overlay_id,
                scope: record.scope.clone(),
                key: line.key.clone(),
                interval: record.interval,
            });
            // **Beneath is the total order, not the integer** (D-220 clause 1,
            // settled by D-249's ascending direction): a strictly lower
            // precedence, **or** an equal precedence with a lower class, since
            // `precedence` is unique only within a class. Collecting only the
            // first is why a `fixed` line discarding an equal-precedence
            // lower-class layer was silently not warned.
            let beneath = record.precedence < candidate.precedence
                || (record.precedence == candidate.precedence
                    && record.scope.class() < candidate.scope.class());
            if beneath {
                collect_lower_layer(&mut facts.layers_beneath, &record, line);
            }
            if record.precedence == candidate.precedence
                && record.scope.class() != candidate.scope.class()
            {
                collect_cross_class_tie(&mut facts.cross_class_ties, &record, line);
            }
        }
    }
    Ok(facts)
}

/// The tie one published line of a **different** class at the same precedence
/// forms with the candidate (D-230).
///
/// One entry per tying overlay, its plan set accumulated across that overlay's
/// lines: the operator's question is *which overlay ties with mine*, so a second
/// line of the same overlay widens the plan list rather than adding a row.
fn collect_cross_class_tie(
    into: &mut Vec<CrossClassTie>,
    holder: &OverlayRecord,
    line: &OverlayLine,
) {
    let mut plans = BTreeSet::new();
    match line.key.plan_id() {
        Some(plan_id) => {
            plans.insert(plan_id);
        }
        None => plans.extend(holder.target_ref.plans.iter().copied()),
    }
    if let Some(existing) = into
        .iter_mut()
        .find(|t| t.price_overlay_id == holder.price_overlay_id)
    {
        existing.plans.extend(plans);
    } else {
        into.push(CrossClassTie {
            price_overlay_id: holder.price_overlay_id,
            class: holder.scope.class(),
            plans,
        });
    }
}

/// The targets one published line beneath the candidate would have a `fixed`
/// layer discard (D-138).
///
/// A list-default line matches **every** target of its own overlay, so it
/// contributes that whole set rather than one plan.
fn collect_lower_layer(into: &mut BTreeSet<PlanId>, holder: &OverlayRecord, line: &OverlayLine) {
    match line.key.plan_id() {
        Some(plan_id) => {
            into.insert(plan_id);
        }
        None => {
            for plan_id in &holder.target_ref.plans {
                into.insert(*plan_id);
            }
        }
    }
}

/// [`OverlayRepo::taxonomy_declares`]' body, taken through any runner.
async fn declares_selector(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    selector: &ScopeSelector,
) -> Result<bool, RepoError> {
    match selector.value() {
        None => Ok(true),
        Some(value) => declares(runner, scope, tenant_id, selector.class(), value).await,
    }
}

/// One published generation's cutover instant, as the **price** plane stores it.
///
/// **The two planes render one instant two ways, and this is the seam.**
/// `pricing_price.cohort` is the canonical scope key's cohort axis and is stored
/// as **epoch milliseconds in text** (`price_repo::read_cohort`, D-144's quantum
/// made storable), with the literal `none` for the two non-grandfathered
/// classes. `pricing_price_overlay_line.cohort` is a `timestamptz`, because §6
/// types it one and because a line's cohort is authored as an instant rather
/// than derived from a key.
///
/// So `inst-plv-eligibility` — *"a `cohort` value that no published
/// `existing_grandfathered` row of the line's target plan carries"* — is a
/// comparison **across two renderings**, and it is done here, once, rather than
/// at the comparison site. Doing it there would mean comparing timestamps as
/// text, which is the trap this crate has a standing rule against: `SeaORM`
/// writes ISO 8601 with a `T`, and `'T'` beats `' '` at byte 11.
///
/// `None` is *"not a generation"* and covers both the `none` sentinel and a
/// token no writer of this crate produces.
fn published_generation(token: &str) -> Option<OffsetDateTime> {
    let millis: i64 = token.parse().ok()?;
    crate::domain::instant::from_unix_millis(millis)
}

/// The highest revision number this overlay has ever minted.
///
/// `max`, not "the published one": a `superseded` predecessor can outrank the
/// published row, so the published row is not the chain's high-water mark. `-1`
/// for an overlay with no rows, so the first successor is `0`.
///
/// **The max of what the table holds is the max the overlay has ever minted**, and
/// D-231 is what makes it so: `chk_pricing_price_overlay_lifecycle_state` admits
/// `abandoned`, which removes `DELETE` as an exit, so a discarded draft is still a row
/// and still counted. Without it this read cannot see the number a deleted draft
/// consumed, and [`OverlayRepo::open_revision`]'s comment traces the consequence.
async fn highest_revision(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_overlay_id: Uuid,
) -> Result<i64, RepoError> {
    let top = price_overlay::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_overlay::Column::TenantId.eq(tenant_id))
                .add(price_overlay::Column::PriceOverlayId.eq(price_overlay_id)),
        )
        .order_by(price_overlay::Column::Revision, Order::Desc)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the overlay's highest revision: {e}")))?;
    Ok(top.map_or(-1, |row| row.revision))
}

/// Is this the line table's identity collision rather than any other fault?
///
/// **The unique class is asked first**, and the message only decides *which*
/// constraint refused. The predicate took a bare `Display` before, so a storage
/// failure of any kind whose text happened to carry this DDL name was answered as
/// a caller-fixable `line_id` refusal. A primary-key violation is in the unique
/// class on both engines — Postgres reports SQLSTATE `23505`, `SQLite` extended
/// code `1555` — so the conjunct costs the real collision nothing.
fn is_line_identity_collision(err: &ScopeError) -> bool {
    err.is_unique_violation() && names_the_line_identity(&err.to_string())
}

/// Does this driver message name the line table's primary key?
///
/// Matched on the primary key's own name (Postgres) and on the column list
/// `SQLite` reports, because the two engines name a primary-key violation
/// differently and neither produces the other's spelling.
fn names_the_line_identity(message: &str) -> bool {
    message.contains("pricing_price_overlay_line_pkey")
        || message.contains("pricing_price_overlay_line.line_id")
}

#[cfg(test)]
#[path = "overlay_repo_tests.rs"]
mod overlay_repo_tests;
