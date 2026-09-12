//! The plan revision chain against a real database.
//!
//! Everything worth proving here is a property of a **statement**, not of a
//! branch in Rust. The compare-and-swap is a conjunction the database evaluates
//! under the row lock, the row-version bump rides inside it, and the partial
//! `UNIQUE` indexes decide what "the current revision" and "the open draft"
//! even mean. A mock would assert that the repository's own `if` fires and
//! would keep asserting it after the predicate that matters had been deleted.
//!
//! The suite also pins the three-way refusal after a failed swap. A repository
//! that answered "conflict" to all three would send an operator into a retry
//! loop for the one case — a frozen revision — where no retry can ever succeed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;

use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::contracts::{
    EntitlementGrants, GrantSet, PlanChangeContract, UsageCounterOnPlanChange,
};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::plan::{PlanRevision, PlanShapePatch};
use bss_pricing::domain::plan_shape::{
    AddonRule, BillingCycle, CompositeMeter, CustomIntervalUnit, DescriptorSet, Frequency,
    PeriodFloorCap, PhaseKind, PlanPhase,
};
use bss_pricing::domain::scope_key::{PhaseId, PlanId, Region};
use bss_pricing::infra::storage::entity::{
    bundle, bundle_component, bundle_revshare, bundle_revshare_group, outbox, plan,
};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::plan_repo::{PlanCounts, count_plans_by_authoring_state};
use bss_pricing::infra::storage::repo::{NewPlanDraft, PlanRepo, PlanShapeRepo};
use bss_pricing::infra::storage::{RepoError, repo_failure};
use time::OffsetDateTime;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use sea_orm_migration::MigratorTrait;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// One value for a whole test binary: these suites drive a repository or a
/// service directly, where the value the HTTP edge would have established has
/// no producer. What each suite asserts *about* it is stated where it asserts
/// it.
const TEST_CORRELATION: uuid::Uuid = uuid::Uuid::from_u128(0x_c0_11_a7_10);

/// The stamp an audited repository call is made under: who acted, when, and the
/// request's correlation.
fn stamp_of(actor: uuid::Uuid, when: OffsetDateTime) -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: actor,
        recorded_at: when,
        correlation_id: TEST_CORRELATION,
    }
}

mod common;

/// The repository, plus the provider the seeding helper needs to put a row into
/// a state `PlanRepo::publish_revision` now reaches by the sanctioned path.
///
/// `retired` no longer needs the seed either: `plan_repo::retire_revision` is
/// its producer as of Slice 11 (D-128), and `tests/sqlite_retirement.rs` drives
/// it. This file keeps `flip_state` because its own subject is the revision
/// chain rather than the act, and reaching `retired` through the orchestrator
/// here would couple a repository suite to a workflow.
async fn harness() -> (PlanRepo, DBProvider<DbError>) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    (PlanRepo::new(provider.clone()), provider)
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 2, hour, 0, 0)
}

/// A draft carrying **every** authorable column, the Slice-2 ones included.
///
/// The frequency is the custom one on purpose: it is the only value whose
/// storage is three columns rather than one, and the only one whose member of
/// `Frequency::ALL` is a placeholder rather than data. A seed that used a fixed
/// frequency would round-trip through a path where the placeholder can never be
/// observed.
fn new_draft(plan_id: PlanId, tenant_id: Uuid) -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id,
        tenant_id,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(10),
        sku_id: Some(Uuid::from_u128(0x5_c1)),
        plan_tier: Some("gold".to_owned()),
        billing_cycle: Some(BillingCycle::Recurring),
        frequency: Some(Frequency::CustomEveryN {
            n: 45,
            unit: CustomIntervalUnit::Days,
        }),
        plan_tier_override: true,
        purchase_min_qty: Some(2),
        purchase_max_qty: Some(10),
        invoice_grouping_key: Some("emea-bundle".to_owned()),
        available_from: Some(at(11)),
        available_to: Some(at(23)),
        cloned_from: None,
        correlation_id: TEST_CORRELATION,
    }
}

/// Move a revision's `lifecycle_state` directly — **a column write, standing in
/// for whichever workflow owns the edge**.
///
/// Used for `draft -> published` at ten call sites and for `published -> retired`
/// beside them. Both edges have producers — `publish::commit` and
/// `plan_repo::retire_revision` (D-128, Slice 11) — and this seed exists so a
/// *repository* suite can put a row in either state in one statement without
/// pulling a workflow into it. It follows that the shape and lifecycle cases below
/// are **not** exercising `publish_revision`: `published_plan_with_shape` and its
/// callers reach `published` by writing the column, and a reader looking for the
/// publish unit's own coverage wants `tests/sqlite_publish_commit.rs`.
///
/// Two triggers whitelist these edges, one per side of `draft`:
/// `trg_pricing_plan_draft_flip_whitelist` fires while OLD is still `draft`
/// (`pricing_plan`) and
/// `trg_pricing_plan_frozen_columns` fires when OLD is past it. Either way the
/// statement moves `lifecycle_state` alone, which is what both whitelists admit.
async fn flip_state(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    plan_id: PlanId,
    revision: i64,
    state: LifecycleState,
) {
    let conn = provider.conn().expect("conn");
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(plan::Column::LifecycleState, Expr::value(state.as_str()))
        .filter(
            Condition::all()
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::Revision.eq(revision)),
        )
        .exec(&conn)
        .await
        .expect("flip the lifecycle state");
    assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
}

#[tokio::test]
async fn a_created_draft_reads_back_whole() {
    let (repo, _provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the first revision");

    // Revision 0, draft, version 0: the three facts every later assertion in
    // this file is measured against.
    assert_eq!(created.revision, 0);
    assert_eq!(created.lifecycle_state, LifecycleState::Draft);
    assert_eq!(created.row_version, RowVersion::new(0));

    let read = repo
        .find_revision(&scope, tenant, plan_id, 0)
        .await
        .expect("read")
        .expect("the revision just created is there");

    // Field for field: a mapping that dropped a column would still round-trip
    // the identity, and the drop would only surface at publish.
    assert_eq!(read, created);
    assert_eq!(read.sku_id, Some(Uuid::from_u128(0x5_c1)));
    assert_eq!(read.plan_tier.as_deref(), Some("gold"));
    assert_eq!(read.billing_cycle, Some(BillingCycle::Recurring));
    assert_eq!(read.available_from, Some(at(11)));
    assert_eq!(read.available_to, Some(at(23)));
    assert_eq!(read.created_by, Uuid::from_u128(0xac_10));
    assert_eq!(read.created_at_utc, at(10));

    // The Slice-2 columns, and one of them is the whole reason this assertion
    // is spelled out rather than left to `read == created`: `frequency` is
    // stored as three columns and `Frequency::ALL`'s `custom_every_n` member
    // carries `CUSTOM_INTERVAL_PLACEHOLDER` — an `n` of 1 — rather than an
    // authored interval. A reader that matched the token against the list and
    // kept what it found would hand back "every 1 day" for every custom plan in
    // the catalog, with nothing anywhere reporting an error.
    assert_eq!(
        read.frequency,
        Some(Frequency::CustomEveryN {
            n: 45,
            unit: CustomIntervalUnit::Days,
        })
    );
    assert!(read.plan_tier_override);
    assert_eq!(read.purchase_min_qty, Some(2));
    assert_eq!(read.purchase_max_qty, Some(10));
    assert_eq!(read.invoice_grouping_key.as_deref(), Some("emea-bundle"));
}

#[tokio::test]
async fn a_plan_whose_only_revision_is_a_draft_has_no_current_one() {
    let (repo, _provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");

    // "Current" is not "the greatest revision number". An unpublished plan has
    // nothing a consumer may resolve, and answering with the draft would hand
    // the projector authoring state (§4.2: consumers never read draft state).
    assert_eq!(
        repo.find_current(&scope, tenant, plan_id)
            .await
            .expect("read current"),
        None
    );
    assert!(
        repo.find_open_draft(&scope, tenant, plan_id)
            .await
            .expect("read open draft")
            .is_some(),
        "the draft itself is still there"
    );
}

#[tokio::test]
async fn an_edit_advances_the_tag_and_the_previous_tag_stops_working() {
    let (repo, _provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");

    let edited = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            PlanShapePatch {
                plan_tier: Some("platinum".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("the first edit holds the current version");
    assert_eq!(edited.plan_tier.as_deref(), Some("platinum"));
    assert_eq!(edited.row_version, RowVersion::new(1));

    // The second writer is the bulk import that read before the interactive
    // edit landed. It is refused, and the refusal names both versions so an
    // operator can tell a caller that never refreshed from a real collision.
    let err = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            PlanShapePatch {
                plan_tier: Some("silver".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect_err("a submit against a superseded tag must be refused");
    assert_eq!(
        err,
        RepoError::StaleRowVersion {
            subject: "plan revision".to_owned(),
            id: format!("{plan_id}/0"),
            current: 1,
            submitted: 0,
        }
    );

    // And nothing of the refused edit landed.
    let read = repo
        .find_revision(&scope, tenant, plan_id, 0)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read.plan_tier.as_deref(), Some("platinum"));
    assert_eq!(read.row_version, RowVersion::new(1));
}

#[tokio::test]
async fn an_empty_patch_is_a_request_and_still_moves_the_tag() {
    let (repo, _provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    // Named, because `new_draft` leaves it unset and a field left at its default
    // cannot show a `None` arm clearing it.
    let seeded = NewPlanDraft {
        plan_name: Some("Platinum EMEA".to_owned()),
        ..new_draft(plan_id, tenant)
    };
    let created = repo.create_draft(&scope, seeded).await.expect("create");

    // `entitlement_grants` and `change_contract` are patch members too, and
    // `NewPlanDraft` carries neither — against the empty defaults a `None` encoded
    // as a clear on either is invisible. One real edit puts non-default values on
    // both axes, and the empty patch below is judged against *that* row.
    let furnished = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            PlanShapePatch {
                entitlement_grants: Some(seeded_grants()),
                change_contract: Some(seeded_change_contract()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("a real edit lands");
    assert_ne!(
        furnished.entitlement_grants, created.entitlement_grants,
        "the control: the empty patch below has something to lose"
    );

    // It asserts the caller's tag and advances it. Treating it as a no-op would
    // make a lost edit and a deliberately empty one indistinguishable to every
    // observer downstream.
    let touched = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(1),
            PlanShapePatch::default(),
            stamp(),
        )
        .await
        .expect("an empty patch is a valid request");

    // **The whole row, not three fields of thirteen.** `PlanShapePatch`'s `None`
    // means leave alone, and the three that were named here are the three whose
    // clearing is least costly; a `None` encoded as a column clear on
    // `billing_cycle`, `purchase_min_qty`, `purchase_max_qty`,
    // `invoice_grouping_key` or `available_from` silently blanks a plan's billing
    // cycle or its availability window, and no case in this file or in
    // `tests/rest_plans.rs` read those columns back after a partial patch.
    // `every_patched_column_reaches_the_row_it_names` moves all ten at once, which
    // is the opposite direction.
    //
    // The tag is the one field that must move, so it is the one substituted rather
    // than excluded — an equality over the rest is a roster the compiler keeps.
    assert_eq!(
        touched,
        PlanRevision {
            row_version: RowVersion::new(2),
            ..furnished
        }
    );
}

#[tokio::test]
async fn a_published_revision_refuses_the_edit_by_name_not_by_trigger() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    flip_state(&provider, &scope, plan_id, 0, LifecycleState::Published).await;

    // The submitted version is the row's real one, so only the draft-only
    // conjunct can have failed. The typed refusal is the whole point: the
    // table trigger would answer with a database error carrying no state, and
    // the caller would be told the store is broken.
    let err = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            PlanShapePatch {
                plan_tier: Some("silver".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect_err("a published revision is frozen in content");
    assert_eq!(
        err,
        RepoError::NotDraft {
            subject: "plan revision".to_owned(),
            id: format!("{plan_id}/0"),
            state: "published".to_owned(),
        }
    );

    // Abandoning it is refused the same way, and again before the trigger.
    let err = repo
        .abandon_draft(&scope, tenant, plan_id, 0, RowVersion::new(0), stamp())
        .await
        .expect_err("only an open draft revision is abandonable");
    assert!(matches!(err, RepoError::NotDraft { .. }));
}

#[tokio::test]
async fn frozen_beats_stale_when_a_write_is_both() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    flip_state(&provider, &scope, plan_id, 0, LifecycleState::Published).await;

    // The real shape of the case: an author holding a tag from before the
    // publish. **Two** conjuncts of the swap failed at once - the version and
    // the draft-only predicate - so the refusal is a precedence decision, not a
    // lookup, and it is the only test that observes which one wins.
    //
    // It has to be `NotDraft`. `StaleRowVersion` reads as "refresh and retry",
    // and no refresh will ever make a published revision editable: the caller
    // would refetch, read version 0, submit again, and be told it is stale
    // again - a loop the surface itself created. The remedy is a different
    // operation (open a new revision), and only the frozen answer says so.
    let err = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(9),
            PlanShapePatch {
                plan_tier: Some("silver".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect_err("a published revision is frozen whatever tag is submitted");
    assert_eq!(
        err,
        RepoError::NotDraft {
            subject: "plan revision".to_owned(),
            id: format!("{plan_id}/0"),
            state: "published".to_owned(),
        },
        "a write that is both frozen and stale must be refused as frozen"
    );

    // `abandon_draft` shares the arms, so it inherits the precedence and has to
    // be held to it too.
    let err = repo
        .abandon_draft(&scope, tenant, plan_id, 0, RowVersion::new(9), stamp())
        .await
        .expect_err("a published revision is unabandonable whatever tag is submitted");
    assert_eq!(
        err,
        RepoError::NotDraft {
            subject: "plan revision".to_owned(),
            id: format!("{plan_id}/0"),
            state: "published".to_owned(),
        },
        "an abandon that is both frozen and stale must be refused as frozen"
    );
}

#[tokio::test]
async fn every_patched_column_reaches_the_row_it_names() {
    let (repo, _provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");

    // Every field-to-column pair in one edit, each moved to a value the seed
    // does not hold. The pairs are hand-wired, and two of them encode through a
    // type the column spells differently on each backend (`uuid` and
    // `timestamptz` on Postgres, `text` on SQLite) - a path the insert does not
    // share. A transposed pair, or a value that silently fails to encode, is
    // invisible to a suite that only ever patches one column.
    let sku_id = Uuid::from_u128(0x5_c2);
    let updated = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            PlanShapePatch {
                plan_name: None,
                entitlement_grants: Option::default(),
                change_contract: Option::default(),
                sku_id: Some(sku_id),
                plan_tier: Some("platinum".to_owned()),
                billing_cycle: Some(BillingCycle::OneTime),
                frequency: Some(Frequency::Annual),
                plan_tier_override: Some(false),
                purchase_min_qty: Some(3),
                purchase_max_qty: Some(4),
                invoice_grouping_key: Some("apac-bundle".to_owned()),
                available_from: Some(at(14)),
                available_to: Some(at(20)),
            },
            stamp(),
        )
        .await
        .expect("a ten-column patch is one edit");

    // Asymmetric on purpose: `available_from` and `available_to` are both moved,
    // to distinct instants, and neither may end up holding the other's. The two
    // purchase bounds are moved to adjacent-but-distinct values for the same
    // reason.
    assert_eq!(updated.sku_id, Some(sku_id));
    assert_eq!(updated.plan_tier.as_deref(), Some("platinum"));
    assert_eq!(updated.billing_cycle, Some(BillingCycle::OneTime));
    assert_eq!(updated.available_from, Some(at(14)));
    assert_eq!(updated.available_to, Some(at(20)));
    assert_eq!(updated.purchase_min_qty, Some(3));
    assert_eq!(updated.purchase_max_qty, Some(4));
    assert_eq!(updated.invoice_grouping_key.as_deref(), Some("apac-bundle"));
    assert_eq!(updated.row_version, RowVersion::new(1));

    // `plan_tier_override` is the one `Option` here over a `NOT NULL` column, so
    // `Some(false)` is a real withdrawal of an audited override (P3) rather than
    // an omission. The seed set it, this patch clears it, and a patch encoding
    // that dropped `Some(false)` as "nothing to do" would leave the override
    // standing while telling the author it was gone.
    assert!(!updated.plan_tier_override);

    // The frequency moved from the custom one to a fixed one, and that is a
    // **three-column** write: `custom_interval_n` and `custom_interval_unit`
    // have to travel to NULL with it. Reading `Some(Annual)` back is the proof —
    // `read_frequency` refuses a fixed frequency that still carries an interval
    // as a corrupt row, so a patch that moved only the token would fail this
    // read instead of returning a value.
    assert_eq!(updated.frequency, Some(Frequency::Annual));

    // And it is the stored row that changed, not just the value handed back.
    let read = repo
        .find_revision(&scope, tenant, plan_id, 0)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read, updated);
}

#[tokio::test]
async fn an_absent_revision_is_not_found_rather_than_stale() {
    let (repo, _provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");

    let err = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            7,
            RowVersion::new(0),
            PlanShapePatch::default(),
            stamp(),
        )
        .await
        .expect_err("revision 7 was never opened");
    assert_eq!(
        err,
        RepoError::NotFound {
            subject: "plan revision".to_owned(),
            id: format!("{plan_id}/7"),
        }
    );
}

#[tokio::test]
async fn a_draft_is_abandoned_only_under_its_own_version_and_the_row_stays() {
    let (repo, _provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");

    // Abandoning a draft is a write like any other: a caller working from a
    // read it did not refresh would otherwise discard an edit it never saw.
    let err = repo
        .abandon_draft(&scope, tenant, plan_id, 0, RowVersion::new(4), stamp())
        .await
        .expect_err("a stale tag must not discard a draft");
    assert_eq!(
        err,
        RepoError::StaleRowVersion {
            subject: "plan revision".to_owned(),
            id: format!("{plan_id}/0"),
            current: 0,
            submitted: 4,
        }
    );

    let tombstone = repo
        .abandon_draft(&scope, tenant, plan_id, 0, RowVersion::new(0), stamp())
        .await
        .expect("the current tag discards");
    assert_eq!(tombstone.lifecycle_state, LifecycleState::Abandoned);
    assert_eq!(
        tombstone.row_version,
        RowVersion::new(1),
        "the representation changed, so the tag moved with it"
    );

    // The row **survives**, which is the whole mechanism: it is what holds the
    // revision number so nothing can mint it a second time. A delete here
    // returned the number to the pool and let a stale tag pass its precondition
    // against a different row wearing the same name.
    let read = repo
        .find_revision(&scope, tenant, plan_id, 0)
        .await
        .expect("read")
        .expect("the tombstone is still there");
    assert_eq!(read, tombstone);
    assert_eq!(
        read.plan_tier.as_deref(),
        Some("gold"),
        "frozen as authored"
    );

    // And it occupies neither of the two slots a plan has, so it disturbs
    // nothing: not the draft slot, not the current revision.
    assert_eq!(
        repo.find_open_draft(&scope, tenant, plan_id)
            .await
            .expect("read open draft"),
        None
    );
    assert_eq!(
        repo.find_current(&scope, tenant, plan_id)
            .await
            .expect("read current"),
        None
    );

    // A second abandon — a retry, or a second operator — is refused as frozen,
    // naming the state. Under a delete this was a not-found, which reads as "no
    // such revision" rather than "that revision is already discarded".
    let err = repo
        .abandon_draft(&scope, tenant, plan_id, 0, RowVersion::new(1), stamp())
        .await
        .expect_err("a tombstone is not an open draft");
    assert_eq!(
        err,
        RepoError::NotDraft {
            subject: "plan revision".to_owned(),
            id: format!("{plan_id}/0"),
            state: "abandoned".to_owned(),
        }
    );
}

#[tokio::test]
async fn an_abandoned_revisions_number_is_never_minted_again() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    flip_state(&provider, &scope, plan_id, 0, LifecycleState::Published).await;

    // Open a successor and discard it. The plan's **current** revision is still
    // 0 — nothing about abandoning revision 1 moved it — so a mint derived from
    // the current revision hands out 1 again, which is the defect D-145 exists
    // to prevent: `plan/1` would name the tombstone *and* the live draft, and a
    // caller holding the tombstone's tag would `PATCH` the new row of that name
    // with a precondition that passes at its initial version.
    let first = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(12)),
        )
        .await
        .expect("the first successor opens");
    assert_eq!(first.revision, 1);
    repo.abandon_draft(&scope, tenant, plan_id, 1, first.row_version, stamp())
        .await
        .expect("discard it");

    let second = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(13)),
        )
        .await
        .expect("the replacement opens straight away");
    assert_ne!(
        second.revision, first.revision,
        "the discarded revision's number must never be minted again"
    );
    assert_eq!(second.revision, 2, "minting is max(revision) + 1");

    // Twice, because one tombstone is also what "one past the current revision
    // plus one" would produce by accident. Two prove the maximum is taken over
    // the whole chain and not over some fixed offset from the current row.
    repo.abandon_draft(&scope, tenant, plan_id, 2, second.row_version, stamp())
        .await
        .expect("discard the replacement too");
    let third = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(14)),
        )
        .await
        .expect("and again");
    assert_eq!(third.revision, 3);

    // The gap is visible and deliberate: rev 0 published, 1 and 2 abandoned,
    // 3 open. Every number ever minted still resolves to the row that minted it.
    for (revision, state) in [
        (0, LifecycleState::Published),
        (1, LifecycleState::Abandoned),
        (2, LifecycleState::Abandoned),
        (3, LifecycleState::Draft),
    ] {
        let row = repo
            .find_revision(&scope, tenant, plan_id, revision)
            .await
            .expect("read")
            .unwrap_or_else(|| panic!("revision {revision} must still be there"));
        assert_eq!(row.lifecycle_state, state, "revision {revision}");
    }
    assert_eq!(
        repo.find_current(&scope, tenant, plan_id)
            .await
            .expect("read current")
            .map(|row| row.revision),
        Some(0),
        "two tombstones later, the current revision is where it was"
    );
    assert_eq!(
        repo.find_open_draft(&scope, tenant, plan_id)
            .await
            .expect("read open draft")
            .map(|row| row.revision),
        Some(3)
    );
}

#[tokio::test]
async fn a_new_revision_copies_the_current_shape_forward() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    // **The three columns `new_draft` cannot author.** `NewPlanDraft` seeds
    // `plan_name: None` and carries no `entitlement_grants`/`change_contract` at
    // all, so until 2026-08-20 the three copies `open_revision` makes at
    // `plan_repo.rs:725/739/740` were only ever compared `None == None` / default
    // == default — and no test in the crate covered those columns across
    // `open_revision`. A copy-forward that dropped `entitlement_grants` strips a
    // plan's quotas and feature flags on the successor revision, and one that
    // dropped `change_contract` drops it out of self-service change.
    let published = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            created.row_version,
            PlanShapePatch {
                plan_name: Some("Gold, EMEA".to_owned()),
                entitlement_grants: Some(seeded_grants()),
                change_contract: Some(seeded_change_contract()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("author the three columns the create form cannot carry");
    flip_state(&provider, &scope, plan_id, 0, LifecycleState::Published).await;

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(12)),
        )
        .await
        .expect("a published plan may open its next revision");

    // The successor is a fresh draft on the same plan: identity unchanged, so
    // the scope-key axis and the price attachment never have to move (§4.3).
    assert_eq!(opened.plan_id, plan_id);
    assert_eq!(opened.revision, 1);
    assert_eq!(opened.lifecycle_state, LifecycleState::Draft);
    assert_eq!(opened.row_version, RowVersion::new(0));
    assert_eq!(opened.created_by, Uuid::from_u128(0xac_20));
    assert_eq!(opened.created_at_utc, at(12));

    // The shape is carried over, so a revision opened to change one field does
    // not silently blank the rest. Every authorable column, not a sample: a
    // copy-forward that missed one would look like an author who had cleared it,
    // and the miss would only surface when the successor published.
    assert_eq!(opened.sku_id, published.sku_id);
    assert_eq!(opened.plan_tier, published.plan_tier);
    assert_eq!(opened.billing_cycle, published.billing_cycle);
    assert_eq!(opened.frequency, published.frequency);
    assert_eq!(opened.plan_tier_override, published.plan_tier_override);
    assert_eq!(opened.purchase_min_qty, published.purchase_min_qty);
    assert_eq!(opened.purchase_max_qty, published.purchase_max_qty);
    assert_eq!(opened.invoice_grouping_key, published.invoice_grouping_key);
    assert_eq!(opened.available_from, published.available_from);
    assert_eq!(opened.available_to, published.available_to);

    // The three that had no operand. Compared against the authored values rather
    // than against `published.*`, so a copy-forward that carried two `None`s
    // faithfully still fails.
    assert_eq!(
        opened.plan_name.as_deref(),
        Some("Gold, EMEA"),
        "the display name comes forward: a successor that lost it appears in the \
         operator's list unnamed"
    );
    assert_eq!(
        opened.entitlement_grants,
        seeded_grants(),
        "the grant set comes forward whole, plan-level flags and quotas together: a \
         successor that dropped it strips the plan's entitlements on publish"
    );
    assert_eq!(
        opened.change_contract,
        seeded_change_contract(),
        "and so does the change contract, D-113's carry flag included"
    );

    // Spelled out rather than left to the pairwise comparisons above, because
    // those hold just as well when both sides are `None`: the seed's custom
    // frequency has to survive the copy **with its interval**, which is the one
    // value a column copy can lose without losing the column.
    assert_eq!(
        opened.frequency,
        Some(Frequency::CustomEveryN {
            n: 45,
            unit: CustomIntervalUnit::Days,
        })
    );
    assert!(opened.plan_tier_override);

    // Both links of the chain are readable, and each is what it should be.
    assert_eq!(
        repo.find_current(&scope, tenant, plan_id)
            .await
            .expect("read current")
            .map(|row| row.revision),
        Some(0)
    );
    assert_eq!(
        repo.find_open_draft(&scope, tenant, plan_id)
            .await
            .expect("read open draft")
            .map(|row| row.revision),
        Some(1)
    );
}

#[tokio::test]
async fn a_plan_gets_one_editable_shape_and_no_more() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    flip_state(&provider, &scope, plan_id, 0, LifecycleState::Published).await;
    repo.open_revision(
        &scope,
        tenant,
        plan_id,
        stamp_of(Uuid::from_u128(0xac_20), at(12)),
    )
    .await
    .expect("the first successor opens");

    // Two concurrently editable shapes on one plan is the state
    // `uq_pricing_plan_open_draft` exists to forbid. The refusal names the
    // revision holding the slot so the caller edits it instead of guessing
    // which of its own requests won.
    let err = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(13)),
        )
        .await
        .expect_err("a second open draft must be refused");
    assert_eq!(
        err,
        RepoError::OpenDraftExists {
            plan_id: plan_id.to_string(),
            revision: 1,
        }
    );
}

#[tokio::test]
async fn a_retired_plan_can_never_open_another_revision() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    flip_state(&provider, &scope, plan_id, 0, LifecycleState::Published).await;
    flip_state(&provider, &scope, plan_id, 0, LifecycleState::Retired).await;

    // Retirement is terminal (D-128): there is no edge out of `retired`, so the
    // predecessor could never flip `superseded` when the successor published.
    // The revision would be unpublishable from the moment it was opened, and
    // refusing at publish time would waste an author's whole editing session.
    //
    // It is **not** `NotDraft`, which answered this case first and whose
    // sentence — "only draft content is mutable" — is about editing a
    // revision's content. Nobody asked to edit anything here, and the remedy
    // that sentence implies is to open the next revision: exactly the call
    // being refused, so a caller following the diagnosis would loop.
    let err = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(12)),
        )
        .await
        .expect_err("a retired plan takes no further revision");
    assert_eq!(
        err,
        RepoError::NoSuccessorRevision {
            plan_id: plan_id.to_string(),
            state: "retired".to_owned(),
        }
    );
}

#[tokio::test]
async fn the_two_refusals_reach_a_consumer_as_two_codes_not_one() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let retired_plan = PlanId::new(Uuid::from_u128(0x9_1a4));
    let occupied_plan = PlanId::new(Uuid::from_u128(0x9_1a5));

    // One plan retired, one plan holding an open draft: the two refusals
    // `LIFECYCLE_FORBIDDEN` used to swallow (D-146). Before the narrowing this
    // test passed with both branches reading `LIFECYCLE_FORBIDDEN`, which is the
    // whole defect — the operator's next action differs and the consumer could
    // not see that it did.
    repo.create_draft(&scope, new_draft(retired_plan, tenant))
        .await
        .expect("create");
    flip_state(
        &provider,
        &scope,
        retired_plan,
        0,
        LifecycleState::Published,
    )
    .await;
    flip_state(&provider, &scope, retired_plan, 0, LifecycleState::Retired).await;

    repo.create_draft(&scope, new_draft(occupied_plan, tenant))
        .await
        .expect("create");
    flip_state(
        &provider,
        &scope,
        occupied_plan,
        0,
        LifecycleState::Published,
    )
    .await;
    repo.open_revision(
        &scope,
        tenant,
        occupied_plan,
        stamp_of(Uuid::from_u128(0xac_20), at(12)),
    )
    .await
    .expect("the first successor opens");

    let stop = repo
        .open_revision(
            &scope,
            tenant,
            retired_plan,
            stamp_of(Uuid::from_u128(0xac_20), at(12)),
        )
        .await
        .expect_err("a retired plan takes no further revision");
    let go_edit = repo
        .open_revision(
            &scope,
            tenant,
            occupied_plan,
            stamp_of(Uuid::from_u128(0xac_20), at(13)),
        )
        .await
        .expect_err("a second open draft must be refused");

    let stop = CanonicalError::from(repo_failure(&stop));
    let go_edit = CanonicalError::from(repo_failure(&go_edit));
    let stop_body = format!("{stop:?}");
    let go_edit_body = format!("{go_edit:?}");

    assert!(
        stop_body.contains("PLAN_RETIRED_NO_SUCCESSOR"),
        "a retired plan is a stop and says so: {stop_body}"
    );
    assert!(
        go_edit_body.contains("OPEN_DRAFT_REVISION_EXISTS"),
        "an occupied draft slot names a real next action: {go_edit_body}"
    );
    assert!(
        !stop_body.contains("LIFECYCLE_FORBIDDEN") && !go_edit_body.contains("LIFECYCLE_FORBIDDEN"),
        "neither may still be told the code that hid the difference"
    );
    // The status carries the same distinction for a client that reads no body:
    // retirement is terminal, an occupied slot is a conflict on mutable state.
    assert_eq!(stop.status_code(), 400);
    assert_eq!(go_edit.status_code(), 409);
    assert!(
        go_edit_body.contains("revision 1"),
        "and the conflict names the revision to go and edit: {go_edit_body}"
    );
}

#[tokio::test]
async fn an_availability_bound_below_the_quantum_is_refused_on_both_write_paths() {
    let (repo, _provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    // `timestamptz` would take this and `SQLite`'s text rendering would too, so
    // nothing below refuses it: the instant persists a resolution finer than the
    // one the catalog compares at (D-144), and the divergence surfaces as a
    // window bound that never matches rather than as an error.
    let mut draft = new_draft(plan_id, tenant);
    draft.available_from = Some(at(11) + time::Duration::microseconds(500));
    let err = repo
        .create_draft(&scope, draft)
        .await
        .expect_err("a sub-millisecond availability bound must be refused");
    assert!(
        matches!(
            &err,
            RepoError::TimestampPrecisionExceeded { field, .. } if field == "availableFrom"
        ),
        "got: {err:?}"
    );
    let mapped = CanonicalError::from(repo_failure(&err));
    assert!(format!("{mapped:?}").contains("TIMESTAMP_PRECISION_EXCEEDED"));
    assert_eq!(
        mapped.status_code(),
        400,
        "an architectural 422 reaches the wire as a 400 carrying its code"
    );

    // Nothing was written, so the plan is still free to be created properly.
    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("clearing the sub-millisecond digits is the whole remedy");

    // And the patch path refuses it too, without moving the row's tag: the
    // create path's check alone would leave the only editable plane open.
    let err = repo
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            PlanShapePatch {
                available_to: Some(at(23) + time::Duration::nanoseconds(1)),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect_err("a sub-millisecond patch must be refused");
    assert!(
        matches!(
            &err,
            RepoError::TimestampPrecisionExceeded { field, .. } if field == "availableTo"
        ),
        "got: {err:?}"
    );
    let read = repo
        .find_revision(&scope, tenant, plan_id, 0)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(
        read.available_to,
        Some(at(23)),
        "the refused edit left nothing"
    );
    assert_eq!(read.row_version, RowVersion::new(0), "and moved no tag");
}

#[tokio::test]
async fn a_plan_with_nothing_published_has_no_revision_to_open_from() {
    let (repo, _provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");

    // The plan exists, but the source a new revision copies from does not. The
    // refusal names the missing referent rather than the plan, because "plan
    // not found" would be false and would send the caller looking for the
    // wrong thing.
    let err = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(12)),
        )
        .await
        .expect_err("there is no current revision to succeed");
    assert_eq!(
        err,
        RepoError::NotFound {
            subject: "current plan revision".to_owned(),
            id: plan_id.to_string(),
        }
    );
}

#[tokio::test]
async fn another_tenants_plan_is_invisible_and_unwritable() {
    let (repo, _provider) = harness().await;
    let mine = Uuid::from_u128(0x7e_11);
    let theirs = Uuid::from_u128(0x7e_22);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&AccessScope::for_tenant(theirs), new_draft(plan_id, theirs))
        .await
        .expect("the other tenant creates its plan");

    // SQL-level BOLA, the same shape `PinFrontierRepo::read` documents: my
    // scope resolves their row to nothing, whichever tenant id I name. The
    // catalog is commercially sensitive, so the reads fail to `None` and the
    // writes fail to "not found" — never to "forbidden", which would confirm
    // the row exists.
    let scope = AccessScope::for_tenant(mine);
    assert_eq!(
        repo.find_revision(&scope, theirs, plan_id, 0)
            .await
            .expect("read"),
        None
    );
    assert_eq!(
        repo.find_current(&scope, theirs, plan_id)
            .await
            .expect("read current"),
        None
    );
    assert_eq!(
        repo.find_open_draft(&scope, theirs, plan_id)
            .await
            .expect("read open draft"),
        None
    );

    let err = repo
        .update_draft(
            &scope,
            theirs,
            plan_id,
            0,
            RowVersion::new(0),
            PlanShapePatch {
                plan_tier: Some("free".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect_err("a foreign draft is not writable");
    assert!(matches!(err, RepoError::NotFound { .. }));

    let err = repo
        .abandon_draft(&scope, theirs, plan_id, 0, RowVersion::new(0), stamp())
        .await
        .expect_err("a foreign draft is not discardable");
    assert!(matches!(err, RepoError::NotFound { .. }));

    let err = repo
        .open_revision(
            &scope,
            theirs,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(12)),
        )
        .await
        .expect_err("a foreign plan takes no revision");
    assert!(matches!(err, RepoError::NotFound { .. }));

    // And the row is untouched for its owner.
    let read = repo
        .find_revision(&AccessScope::for_tenant(theirs), theirs, plan_id, 0)
        .await
        .expect("read")
        .expect("their revision is still there");
    assert_eq!(read.plan_tier.as_deref(), Some("gold"));
    assert_eq!(read.row_version, RowVersion::new(0));
}

#[tokio::test]
async fn a_plan_may_not_be_created_into_another_tenant() {
    let (repo, _provider) = harness().await;
    let mine = Uuid::from_u128(0x7e_11);
    let theirs = Uuid::from_u128(0x7e_22);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    // The insert side of the same BOLA the reads, updates and deletes above
    // close, and the only one that no `WHERE` clause can: a scoped read filters
    // rows that exist, while a scoped insert has to refuse a row before it
    // does. `scope_with_model` is what refuses it — the `ActiveModel`'s own
    // `tenant_id` is checked against the caller's scope — and without that
    // check a caller could plant a plan inside another tenant's catalog and
    // then be unable to see, edit or delete what it had written.
    let err = repo
        .create_draft(&AccessScope::for_tenant(mine), new_draft(plan_id, theirs))
        .await
        .expect_err("a plan may not be created into a tenant the caller is not scoped to");
    let RepoError::Db(detail) = err else {
        panic!("a refused insert scope is a storage failure, not a typed refusal");
    };
    assert!(detail.contains("pricing_plan scope"), "got: {detail}");

    // And nothing landed under either tenant — least of all the victim's.
    assert_eq!(
        repo.find_revision(&AccessScope::for_tenant(theirs), theirs, plan_id, 0)
            .await
            .expect("read"),
        None
    );
    assert_eq!(
        repo.find_revision(&AccessScope::for_tenant(mine), mine, plan_id, 0)
            .await
            .expect("read"),
        None
    );
}

/// **A composite-set request naming a tenant the parent revision does not carry
/// writes nothing, under either scope** (Z7-4, Global Constraint 9).
///
/// The composite plane was the one shape table with no cross-tenant case, and it
/// is the plane whose `_on` body took the child rows' `tenant_id` and `plan_id`
/// from the **request** where its three siblings take them off the parent revision
/// row. This case is what makes the unreachability argument a measured fact rather
/// than a reading of `load_revision_row`: the request's tenant is not what selects
/// the parent, so a divergent one resolves no draft and the refusal lands before
/// any child row is rendered. It therefore passes with and without the parent
/// binding, and is **not** the fix's probe — the fix's gate is the compiler, since
/// `write_composites` no longer has request values to be handed. Stated outright
/// so no later reader mistakes it for one.
#[tokio::test]
async fn a_composite_set_request_naming_a_foreign_tenant_writes_nothing() {
    let (repo, provider) = harness().await;
    let owner = Uuid::from_u128(0x7e_11);
    let intruder = Uuid::from_u128(0x7e_22);
    let owner_scope = AccessScope::for_tenant(owner);
    let intruder_scope = AccessScope::for_tenant(intruder);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let shapes = PlanShapeRepo::new(provider.clone());

    repo.create_draft(&owner_scope, new_draft(plan_id, owner))
        .await
        .expect("the owner opens revision 0");

    // The owner's own scope, naming somebody else's tenant: the parent row
    // carries `owner`, the request says `intruder`.
    let err = shapes
        .replace_composites(
            &owner_scope,
            intruder,
            plan_id,
            0,
            RowVersion::new(0),
            two_composites(),
            stamp(),
        )
        .await
        .expect_err("a request naming another tenant resolves no parent revision");
    assert!(matches!(err, RepoError::NotFound { .. }), "got: {err:?}");
    // And the mirror direction: a foreign scope naming the owning tenant.
    let err = shapes
        .replace_composites(
            &intruder_scope,
            owner,
            plan_id,
            0,
            RowVersion::new(0),
            two_composites(),
            stamp(),
        )
        .await
        .expect_err("naming the owning tenant does not widen a foreign scope");
    assert!(matches!(err, RepoError::NotFound { .. }), "got: {err:?}");

    // No child row landed under the tenant the request named. A scoped read is
    // the right instrument for that: a row carrying `intruder` in `tenant_id`
    // would be visible to exactly this scope and invisible to the owner.
    assert!(
        shapes
            .list_composites(&intruder_scope, intruder, plan_id, 0)
            .await
            .expect("read")
            .is_empty(),
        "nothing may land under the tenant a request named"
    );
    // The positive control: the same call for the parent's own tenant writes the
    // set, so the refusals above are not a fixture that refuses everything.
    shapes
        .replace_composites(
            &owner_scope,
            owner,
            plan_id,
            0,
            RowVersion::new(0),
            two_composites(),
            stamp(),
        )
        .await
        .expect("the owner's own request writes the set");
    assert_eq!(
        shapes
            .list_composites(&owner_scope, owner, plan_id, 0)
            .await
            .expect("read"),
        two_composites(),
        "and the owner sees both rows"
    );
}

/// **A composite id belongs to a plan, not to a revision number** — A1-1, D-340's
/// class one table over.
///
/// `pricing_composite_meter`'s primary key was `(composite_id, plan_revision)`
/// with a **client-supplied** `composite_id`
/// (`api/rest/plans.rs`: `view.composite_id.unwrap_or_else(Uuid::now_v7)`) and
/// neither `tenant_id` nor `plan_id` in it — `pricing_plan_phase`'s pre-fix key
/// exactly, which the migration's own module doc named as the resemblance it was
/// modelled on. So one composite id belonged to one plan **per revision number
/// across the whole table**, every tenant's included: the first tenant to take an
/// id at revision `0` locked every other tenant out of it at that number for good,
/// and the difference between the refusal and the `200` answered *is this id in
/// use somewhere I cannot read*.
///
/// Two tenants, two plans, one id, the same revision number — the shape the old
/// key could not represent. Both drafts are written and both read back whole,
/// because a widening that let the row land and lost it in the read would look
/// identical here to one that worked.
#[tokio::test]
async fn two_tenants_may_hold_one_composite_id_at_one_revision_number() {
    let (repo, provider) = harness().await;
    let first = Uuid::from_u128(0x7e_11);
    let second = Uuid::from_u128(0x7e_22);
    let first_scope = AccessScope::for_tenant(first);
    let second_scope = AccessScope::for_tenant(second);
    let first_plan = PlanId::new(Uuid::from_u128(0x9_1a4));
    let second_plan = PlanId::new(Uuid::from_u128(0x9_1a5));
    let shapes = PlanShapeRepo::new(provider.clone());

    repo.create_draft(&first_scope, new_draft(first_plan, first))
        .await
        .expect("the first tenant opens revision 0");
    repo.create_draft(&second_scope, new_draft(second_plan, second))
        .await
        .expect("the second tenant opens revision 0");

    shapes
        .replace_composites(
            &first_scope,
            first,
            first_plan,
            0,
            RowVersion::new(0),
            two_composites(),
            stamp(),
        )
        .await
        .expect("the first tenant authors its composite set");
    shapes
        .replace_composites(
            &second_scope,
            second,
            second_plan,
            0,
            RowVersion::new(0),
            // The **same ids**, at the same revision number, in another tenant.
            two_composites(),
            stamp(),
        )
        .await
        .expect("a composite id is not a name in a deployment-wide namespace");

    assert_eq!(
        shapes
            .list_composites(&first_scope, first, first_plan, 0)
            .await
            .expect("read"),
        two_composites(),
        "the first tenant still holds its own set whole"
    );
    assert_eq!(
        shapes
            .list_composites(&second_scope, second, second_plan, 0)
            .await
            .expect("read"),
        two_composites(),
        "and the second holds its own, under the same ids"
    );

    // **The two writes above vary `tenant_id` and `plan_id` together**, so a key of
    // `(plan_id, plan_revision, composite_id)` — which leaves the oracle open on
    // any table where two tenants can share a plan id — passes them unchanged. The
    // axis is separated by the stored key's own column list, and nothing else in
    // this file pinned it. A same-plan two-tenant world cannot be authored instead:
    // `pricing_plan`'s key is `(plan_id, revision)`, so two tenants cannot both
    // hold the plan these composites hang off.
    //
    // Read off a second database built by the same chain, because the DDL is a
    // property of the schema and this suite's provider exposes no raw connection.
    let ddl = common::scalar(
        &common::migrated_db().await,
        "SELECT sql AS v FROM sqlite_master \
         WHERE type = 'table' AND name = 'pricing_composite_meter'",
    )
    .await;
    assert!(
        ddl.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("PRIMARY KEY (tenant_id, plan_id, plan_revision, composite_id)"),
        "`tenant_id` must be in the composite key's column list, or the cross-tenant oracle this \
         case is named for is still open: {ddl}"
    );
}

/// What the widening deliberately keeps: **one revision may still not hold the
/// same composite id twice**.
///
/// The negative control for the case above, and green before the widening as well
/// as after — which is what makes it a control rather than a second probe. The
/// half that had to survive: `list_composites` returns a set the self-reference
/// walk and the arity rule quantify over, and both are written as though a
/// composite id names at most one row of a revision. A widening that also admitted
/// the duplicate would have satisfied A1-1 and quietly made those rules judge a
/// set nobody can author.
#[tokio::test]
async fn one_revision_still_may_not_hold_one_composite_id_twice() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let shapes = PlanShapeRepo::new(provider.clone());

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("the tenant opens revision 0");

    let mut twice = two_composites();
    twice[1].composite_id = twice[0].composite_id;
    twice[1].output_unit = "vm-hour".to_owned();

    let err = shapes
        .replace_composites(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            twice,
            stamp(),
        )
        .await
        .expect_err("one revision, one row per composite id");
    assert!(matches!(err, RepoError::Db(_)), "got: {err:?}");
    assert!(
        shapes
            .list_composites(&scope, tenant, plan_id, 0)
            .await
            .expect("read")
            .is_empty(),
        "and the refused set left nothing behind"
    );
}

/// The one pairing the table cannot refuse, refused where the row is read.
///
/// `chk_pricing_plan_custom_interval_pairing` compares
/// `frequency = 'custom_every_n'` against "**both** interval columns present",
/// so a row carrying only one of them under a fixed or absent frequency has a
/// false right-hand side and satisfies the CHECK. Nothing in the schema is left
/// to catch it, which makes `read_frequency`'s whitelist of the two legal
/// shapes the only guard standing — and a guard with no test behind it is one
/// that gets simplified away by the next reader who finds it redundant.
///
/// Both arms are exercised, because the reading has two: an absent `frequency`
/// and a fixed one are different branches with the same obligation.
#[tokio::test]
async fn a_half_set_interval_the_check_admits_is_refused_where_it_is_read() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    let conn = provider.conn().expect("conn");

    for frequency in [Some("monthly".to_owned()), None] {
        // Column-level, the way `flip_state` seeds a state the repository
        // cannot reach: the typed path is exactly what cannot express this row,
        // because `Frequency` carries the interval inside the variant and no
        // `NewPlanDraft` or `PlanShapePatch` can separate them. The draft plane
        // is unguarded in its columns, so the write lands.
        let moved = plan::Entity::update_many()
            .secure()
            .scope_with(&scope)
            .col_expr(plan::Column::Frequency, Expr::value(frequency.clone()))
            .col_expr(plan::Column::CustomIntervalN, Expr::value(Some(3_i32)))
            .col_expr(
                plan::Column::CustomIntervalUnit,
                Expr::value(Option::<String>::None),
            )
            .filter(
                Condition::all()
                    .add(plan::Column::PlanId.eq(plan_id.get()))
                    .add(plan::Column::Revision.eq(0_i64)),
            )
            .exec(&conn)
            .await
            .unwrap_or_else(|e| {
                panic!("the CHECK admits a half-set pair under {frequency:?}: {e}")
            });
        assert_eq!(moved.rows_affected, 1);

        // An interval nothing can interpret is an invariant breach, not a
        // caller's mistake: the row could only have been written by something
        // that reached the table outside this gear. Reading it back as
        // `Some(Monthly)` would silently discard an authored interval; reading
        // it as a not-found would send an author looking for a revision that
        // is right there.
        let err = repo
            .find_revision(&scope, tenant, plan_id, 0)
            .await
            .expect_err("an orphaned interval must not read back as a plan");
        assert!(
            matches!(err, RepoError::CorruptRow(_)),
            "frequency {frequency:?}, got: {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// D-83 / D-145 against all three revision-scoped child tables.
//
// The anchor that stood here while the tables were landing named the tables that
// did not exist yet, and it went green as each one arrived. Its successor names
// the **property** instead - the set of revision-scoped tables is closed at
// three - so it turns red on the fourth, which is exactly when its author needs
// to be told what else the table owes. See
// `the_revision_scoped_tables_are_a_closed_set_and_each_one_is_copied_and_dropped`.
// ---------------------------------------------------------------------------

/// A three-phase chain: trial -> intro -> evergreen, the last one terminal.
///
/// The ordinals are authored ascending and the ids are not, so a copy that
/// re-minted ids or a read that returned rows in insertion order would both be
/// visible in the assertions rather than hidden by a lucky ordering.
fn three_phases() -> Vec<PlanPhase> {
    let trial = PhaseId::new(Uuid::from_u128(0xf13));
    let intro = PhaseId::new(Uuid::from_u128(0xf11));
    let evergreen = PhaseId::new(Uuid::from_u128(0xf12));
    vec![
        PlanPhase {
            phase_id: trial,
            kind: PhaseKind::Trial,
            display_name: None,
            ordinal: 0,
            converts_to_phase_id: Some(intro),
            phase_duration_days: Some(14),
            // The projection, equal to its source: the table's CHECK is what
            // stops the two persisted columns drifting.
            display_trial_days: Some(14),
        },
        PlanPhase {
            phase_id: intro,
            kind: PhaseKind::Interim,
            // **Non-`None` on purpose (D-357).** Every phase fixture that crosses
            // a revision copy or a clone used to set this `None`, so
            // `copy_phases`' `display_name: Set(row.display_name)` and
            // `clone.rs::remapped_phase` were indistinguishable from `Set(None)`
            // and the field's whole storage contract - that it versions with the
            // revision like every other phase field - was asserted nowhere.
            display_name: Some("Onboarding".to_owned()),
            ordinal: 1,
            converts_to_phase_id: Some(evergreen),
            phase_duration_days: Some(30),
            display_trial_days: None,
        },
        PlanPhase {
            phase_id: evergreen,
            kind: PhaseKind::Evergreen,
            display_name: None,
            ordinal: 2,
            // Terminality is the absent successor, never the kind.
            converts_to_phase_id: None,
            phase_duration_days: None,
            display_trial_days: None,
        },
    ]
}

/// The three add-on rules §9's D-105 acceptance case names, with one conflict
/// edge authored on one side and one `depends_on` edge.
///
/// Three rather than one because that is the number the earlier
/// `(plan_id, plan_revision)` key could not hold: with one rule per revision the
/// cycle walk has no edge to walk and the conflict pair has no second side.
fn three_addon_rules() -> Vec<AddonRule> {
    let analytics = Uuid::from_u128(0xadd01);
    let support = Uuid::from_u128(0xadd02);
    let seats = Uuid::from_u128(0xadd03);
    vec![
        AddonRule {
            addon_sku_id: analytics,
            required: true,
            min_qty: Some(1),
            max_qty: Some(3),
            step_qty: Some(1),
            price_override_ref: None,
            depends_on: Vec::new(),
            conflicts_with: vec![support],
        },
        AddonRule {
            addon_sku_id: support,
            required: false,
            min_qty: None,
            max_qty: None,
            step_qty: None,
            price_override_ref: None,
            depends_on: Vec::new(),
            // Authored empty on purpose: the repository closes the conflict
            // under symmetry, so this row must read back naming analytics.
            conflicts_with: Vec::new(),
        },
        AddonRule {
            addon_sku_id: seats,
            required: false,
            min_qty: None,
            max_qty: None,
            step_qty: None,
            price_override_ref: Some(Uuid::from_u128(0x0_ff1)),
            depends_on: vec![analytics],
            conflicts_with: Vec::new(),
        },
    ]
}

/// Create a plan, author `three_phases()` and `three_addon_rules()` on its
/// revision 0, and publish it.
///
/// Returns the shape repository, so a caller can read either revision back.
///
/// Both child sets are authored, not just the one a given case asserts on: the
/// obligations under test are `open_revision` copying **every** child table
/// forward and `abandon_draft` dropping **every** one, and a seed carrying a
/// single table would leave a copier that handles exactly one of them green.
async fn published_plan_with_shape(
    repo: &PlanRepo,
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
) -> PlanShapeRepo {
    let shapes = PlanShapeRepo::new(provider.clone());
    repo.create_draft(scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    shapes
        .replace_phases(
            scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            three_phases(),
            stamp(),
        )
        .await
        .expect("author the phase chain on the open draft");
    // Under the tag the phase write advanced to: a revision's child sets share
    // the revision's entity tag, so authoring the second one is an edit against
    // the version the first one produced.
    shapes
        .replace_addon_rules(
            scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(1),
            three_addon_rules(),
            stamp(),
        )
        .await
        .expect("author the add-on set on the open draft");
    shapes
        .set_descriptor_set(
            scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(2),
            descriptors(),
            stamp(),
        )
        .await
        .expect("attach the descriptor set on the open draft");
    shapes
        .replace_composites(
            scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(3),
            two_composites(),
            stamp(),
        )
        .await
        .expect("author the composite set on the open draft");
    shapes
        .replace_period_floor_caps(
            scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(4),
            two_period_bounds(),
            stamp(),
        )
        .await
        .expect("author the period floor/cap set on the open draft");
    seed_bundle_composition(provider, scope, tenant, plan_id).await;
    flip_state(provider, scope, plan_id, 0, LifecycleState::Published).await;
    shapes
}

/// Two period bounds, one per market, in the order the reader guarantees —
/// `(currency, region)` — so the assertions stay equalities over a total order
/// (**D-319**).
fn two_period_bounds() -> Vec<PeriodFloorCap> {
    vec![
        PeriodFloorCap {
            currency: CurrencyCode::new("EUR").expect("three letters"),
            region: Region::new("DE").expect("non-blank"),
            floor_minor: Some(MinorAmount::new(40_000).expect("non-negative")),
            cap_minor: None,
        },
        PeriodFloorCap {
            currency: CurrencyCode::new("USD").expect("three letters"),
            region: Region::new("US").expect("non-blank"),
            floor_minor: Some(MinorAmount::new(50_000).expect("non-negative")),
            cap_minor: Some(MinorAmount::new(500_000).expect("non-negative")),
        },
    ]
}

/// Two composite definitions, neither self-referential, for the shape cases.
///
/// **In the order the reader guarantees** — `output_unit` then `composite_id` —
/// so the assertions stay equalities over a total order rather than set
/// comparisons, which is `three_phases`' arrangement for the same reason.
fn two_composites() -> Vec<CompositeMeter> {
    vec![
        CompositeMeter {
            composite_id: Uuid::from_u128(0xc0_a2),
            output_unit: "storage-unit".to_owned(),
            constituent_units: vec!["iops".to_owned(), "gb-month".to_owned()],
            formula: serde_json::json!({ "op": "weighted_sum", "weights": [1, 1] }),
        },
        CompositeMeter {
            composite_id: Uuid::from_u128(0xc0_a1),
            output_unit: "vm-hour".to_owned(),
            constituent_units: vec!["vcpu-hour".to_owned(), "ram-gb-hour".to_owned()],
            formula: serde_json::json!({ "op": "weighted_sum", "weights": [1, 4] }),
        },
    ]
}

/// The plan this fixture builds is also a **bundle**, and its composition is
/// authored on the open draft like every other part of the shape.
///
/// Through the entities rather than a repository: Slice 8's authoring surface is
/// not what these two cases are about, and what they must see is rows under
/// revision 0 that `open_revision` then has to copy and `abandon_draft` then has
/// to drop. Two components, one rev-share group and two parties in it — enough
/// that a copy which carried *a* row while losing the set would be visible.
async fn seed_bundle_composition(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
) {
    let conn = provider.conn().expect("conn");
    let bundle = bundle_id_of(plan_id);
    let vendor = Uuid::from_u128(0x00be_110d);

    let row = bundle::ActiveModel {
        bundle_id: Set(bundle),
        tenant_id: Set(tenant),
        plan_id: Set(plan_id.get()),
        price_basis: Set("sum_of_parts".to_owned()),
        invoice_itemization: Set("itemize".to_owned()),
    };
    bundle::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .expect("scope the bundle")
        .exec(&conn)
        .await
        .expect("seed pricing_bundle");

    for (component, sku) in [
        (Uuid::from_u128(0x0c01), Uuid::from_u128(0x05c1)),
        (Uuid::from_u128(0x0c02), Uuid::from_u128(0x05c2)),
    ] {
        let row = bundle_component::ActiveModel {
            bundle_id: Set(bundle),
            plan_revision: Set(0),
            component_plan_id: Set(component),
            tenant_id: Set(tenant),
            included_sku_id: Set(sku),
            // **Non-null**, so the copy has something to lose. Seeded `None` until
            // 2026-08-20, under which no assertion on these two columns could see
            // a `copy_composition` that defaulted them.
            min_qty: Set(Some(1)),
            max_qty: Set(Some(4)),
        };
        bundle_component::Entity::insert(row.clone())
            .secure()
            .scope_with_model(scope, &row)
            .expect("scope the component")
            .exec(&conn)
            .await
            .expect("seed pricing_bundle_component");
    }

    let row = bundle_revshare_group::ActiveModel {
        bundle_id: Set(bundle),
        plan_revision: Set(0),
        vendor_sku_id: Set(vendor),
        tenant_id: Set(tenant),
        platform_cut_bp: Set(1000),
        residual_absorber_party: Set("platform".to_owned()),
    };
    bundle_revshare_group::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .expect("scope the group")
        .exec(&conn)
        .await
        .expect("seed pricing_bundle_revshare_group");

    for party in ["vendor-a", "vendor-b"] {
        let row = bundle_revshare::ActiveModel {
            bundle_id: Set(bundle),
            plan_revision: Set(0),
            vendor_sku_id: Set(vendor),
            party: Set(party.to_owned()),
            tenant_id: Set(tenant),
            share_bp: Set(4500),
            // Set here so the copy has something to drop: a published revision's
            // parties carry the normalization, and the successor's must not.
            effective_share_bp: Set(Some(4500)),
        };
        bundle_revshare::Entity::insert(row.clone())
            .secure()
            .scope_with_model(scope, &row)
            .expect("scope the party")
            .exec(&conn)
            .await
            .expect("seed pricing_bundle_revshare");
    }
}

/// The bundle id this fixture gives a plan. Derived so the assertions can name
/// it without threading a value through every caller.
fn bundle_id_of(plan_id: PlanId) -> Uuid {
    Uuid::from_u128(plan_id.get().as_u128() ^ 0x000b_0d1e)
}

/// Every component row of one revision, **in `component_plan_id` order**.
///
/// Returns the models rather than a count, for [`party_rows`]' reason: a count
/// cannot see `component_plan_id`, `included_sku_id`, `min_qty`, `max_qty` — so a
/// `copy_composition` that carried the right *number* of rows with a defaulted or
/// transposed field republished the bundle differently and stayed green.
async fn component_rows(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    plan_id: PlanId,
    revision: i64,
) -> Vec<bundle_component::Model> {
    let conn = provider.conn().expect("conn");
    bundle_component::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bundle_component::Column::BundleId.eq(bundle_id_of(plan_id)))
                .add(bundle_component::Column::PlanRevision.eq(revision)),
        )
        .order_by(bundle_component::Column::ComponentPlanId, Order::Asc)
        .all(&conn)
        .await
        .expect("read components")
}

/// Every rev-share group row of one revision, **in `vendor_sku_id` order**.
///
/// The models, for [`component_rows`]' reason: `platform_cut_bp` is the platform's
/// take of a third party's money, and a count says nothing about it.
async fn group_rows(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    plan_id: PlanId,
    revision: i64,
) -> Vec<bundle_revshare_group::Model> {
    let conn = provider.conn().expect("conn");
    bundle_revshare_group::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bundle_revshare_group::Column::BundleId.eq(bundle_id_of(plan_id)))
                .add(bundle_revshare_group::Column::PlanRevision.eq(revision)),
        )
        .order_by(bundle_revshare_group::Column::VendorSkuId, Order::Asc)
        .all(&conn)
        .await
        .expect("read groups")
}

/// Every rev-share party row of one revision.
async fn party_rows(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    plan_id: PlanId,
    revision: i64,
) -> Vec<bundle_revshare::Model> {
    let conn = provider.conn().expect("conn");
    bundle_revshare::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bundle_revshare::Column::BundleId.eq(bundle_id_of(plan_id)))
                .add(bundle_revshare::Column::PlanRevision.eq(revision)),
        )
        .all(&conn)
        .await
        .expect("read parties")
}

/// A grant set worth copying forward: a tier reference, a plan-level feature flag
/// and quota, and a per-phase set carrying one of each - no field at its default.
fn seeded_grants() -> EntitlementGrants {
    EntitlementGrants {
        // Non-default on **every** field, which is the point: a copier or a
        // round-trip asserted against `None` and an empty map is asserted against
        // the values a skipped field also produces, so neither can tell a copier
        // that carried the column from one that dropped it.
        plan_tier_ref: Some("gold".to_owned()),
        plan_level: GrantSet {
            feature_flags: BTreeMap::from([("bss.pricing/api".to_owned(), true)]),
            quotas: BTreeMap::from([("projects".to_owned(), 5)]),
        },
        per_phase: BTreeMap::from([(
            three_phases()[0].phase_id.get(),
            GrantSet {
                feature_flags: BTreeMap::from([("bss.pricing/trial".to_owned(), false)]),
                quotas: BTreeMap::from([("seats".to_owned(), 2)]),
            },
        )]),
    }
}

/// A change contract worth copying forward: an edge list, a rank, and D-113's
/// carry flag off its default.
fn seeded_change_contract() -> PlanChangeContract {
    PlanChangeContract {
        allowed_change_targets: Some(vec![Uuid::from_u128(0x9_1a9)]),
        comparability_rank: Some(10),
        usage_counter_on_plan_change: UsageCounterOnPlanChange::Carry,
    }
}

/// A complete v1 descriptor set plus one P5 extra field.
fn descriptors() -> DescriptorSet {
    DescriptorSet {
        invoice_line_template: Some("Subscription: {plan}".to_owned()),
        gl_code: Some("4000".to_owned()),
        itemization_rule: Some("per_plan".to_owned()),
        additional: BTreeMap::from([("costCentre".to_owned(), "emea-ops".to_owned())]),
    }
}

#[tokio::test]
async fn a_new_revision_carries_its_predecessors_phases_with_the_same_ids_d83() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let shapes = published_plan_with_shape(&repo, &provider, &scope, tenant, plan_id).await;

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_11), at(12)),
        )
        .await
        .expect("open the successor revision");
    assert_eq!(opened.revision, 1);

    // Field for field, including the `phase_id`s. Stable ids are the whole
    // point: the `phase` axis of the canonical scope key holds a bare phase id,
    // so a re-minted one would move every continuing price row onto a key
    // nothing is filed under and same-key supersession would stop matching.
    let carried = shapes
        .list_phases(&scope, tenant, plan_id, 1)
        .await
        .expect("read the new revision's phases");
    assert_eq!(
        carried,
        three_phases(),
        "the successor revision must carry the whole chain, ids and all"
    );

    // And the published revision still holds its own copies: the copy is a
    // copy, not a move, and the frozen revision's shape is what the projector
    // re-reads at every warm re-drive.
    let frozen = shapes
        .list_phases(&scope, tenant, plan_id, 0)
        .await
        .expect("read the published revision's phases");
    assert_eq!(
        frozen,
        three_phases(),
        "the published revision's copies must be untouched"
    );
}

#[tokio::test]
async fn an_abandoned_revision_keeps_none_of_its_phase_copies_d145() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let shapes = published_plan_with_shape(&repo, &provider, &scope, tenant, plan_id).await;

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_11), at(12)),
        )
        .await
        .expect("open the successor revision");
    assert_eq!(
        shapes
            .list_phases(&scope, tenant, plan_id, 1)
            .await
            .expect("read")
            .len(),
        3,
        "the draft must start from its predecessor's chain"
    );

    let tombstone = repo
        .abandon_draft(&scope, tenant, plan_id, 1, opened.row_version, stamp())
        .await
        .expect("abandon the draft revision");
    assert_eq!(tombstone.lifecycle_state, LifecycleState::Abandoned);

    // A tombstone that kept its shape would leave a frozen phase set hanging
    // off a revision no path can reach, and the number it consumed would still
    // name a shape somebody could read.
    assert!(
        shapes
            .list_phases(&scope, tenant, plan_id, 1)
            .await
            .expect("read")
            .is_empty(),
        "an abandoned revision must hold no phase rows"
    );
    assert_eq!(
        shapes
            .list_phases(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        three_phases(),
        "the published revision's chain must still stand"
    );
}

/// §9's D-105 acceptance case: "A plan carrying **three** add-on rules
/// round-trips: all three persist under the revision, the `depends_on` cycle
/// walk sees all three edges, and a draft revision's edit copies all three under
/// the new `plan_revision`."
///
/// Three is the load-bearing number. The pre-D-105 key admitted one rule per
/// revision, so a suite exercising a single add-on would have passed against a
/// table that cannot express a dependency edge at all.
#[tokio::test]
async fn a_new_revision_carries_all_three_add_on_rules_d105() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let shapes = published_plan_with_shape(&repo, &provider, &scope, tenant, plan_id).await;

    let published = shapes
        .list_addon_rules(&scope, tenant, plan_id, 0)
        .await
        .expect("read the published revision's add-on rules");
    assert_eq!(published.len(), 3, "all three persist under the revision");

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_11), at(12)),
        )
        .await
        .expect("open the successor revision");
    assert_eq!(opened.revision, 1);

    // Field for field under the new `plan_revision`, the edges included: the
    // cycle walk on the successor has to see the same graph the predecessor's
    // author drew, or a revision opened to change a price silently changes what
    // the plan composes with.
    let carried = shapes
        .list_addon_rules(&scope, tenant, plan_id, 1)
        .await
        .expect("read the new revision's add-on rules");
    assert_eq!(carried, published, "the successor carries the whole set");
    assert_eq!(
        carried
            .iter()
            .map(|row| row.depends_on.len() + row.conflicts_with.len())
            .sum::<usize>(),
        // One `depends_on` edge, and the conflict on both of its ends.
        3,
        "every authored edge, and the back-edge symmetry added on write"
    );

    // And the published revision keeps its own copies: the copy is a copy, not a
    // move, and the frozen revision's composition is what the projector re-reads
    // at every warm re-drive.
    assert_eq!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        published,
        "the published revision's copies must be untouched"
    );
}

#[tokio::test]
async fn an_abandoned_revision_keeps_none_of_its_add_on_rules_d145() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let shapes = published_plan_with_shape(&repo, &provider, &scope, tenant, plan_id).await;

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_11), at(12)),
        )
        .await
        .expect("open the successor revision");
    assert_eq!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 1)
            .await
            .expect("read")
            .len(),
        3,
        "the draft must start from its predecessor's composition"
    );

    repo.abandon_draft(&scope, tenant, plan_id, 1, opened.row_version, stamp())
        .await
        .expect("abandon the draft revision");

    // The drop has to precede the flip: `abandoned` is not `draft`, and the
    // table's DELETE trigger refuses everything afterwards. A tombstone that
    // kept its rules would leave a frozen composition hanging off a revision no
    // path can reach.
    assert!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 1)
            .await
            .expect("read")
            .is_empty(),
        "an abandoned revision must hold no add-on rules"
    );
    assert_eq!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 0)
            .await
            .expect("read")
            .len(),
        3,
        "the published revision's composition must still stand"
    );
}

// ---------------------------------------------------------------------------
// The whole shape, and the argument the retired anchor was holding.
// ---------------------------------------------------------------------------

/// A revision copies **every** revision-scoped child table it has, and the two
/// cases below are what stands behind that now.
///
/// They replace a schema anchor. While the three child tables were landing one
/// per task, `no_child_shape_table_exists_yet_for_a_new_revision_to_copy_d83`
/// queried `sqlite_master` and failed the moment a table it named appeared —
/// deliberately interrogating the schema rather than migration names, because a
/// migration named for its slice rather than its tables
/// (`m..._create_slice2_plan_shape`) would have created several and left a
/// name-matching anchor green. Its purpose was to make "add revision-scoped
/// storage" and "teach `open_revision` to copy it and `abandon_draft` to drop
/// it" one indivisible piece of work.
///
/// Slice 2 owns no fourth table, so the anchor has nothing left to name and is
/// gone. **The obligation is not**: a later slice adding a revision-scoped child
/// gets no compiler error and no failing anchor, so these two cases are where
/// its copy and its drop have to be asserted, and a reader who adds a table
/// without touching this file has left the same gap the anchor existed to close.
#[tokio::test]
async fn a_new_revision_carries_the_whole_shape_forward_with_stable_ids_d83() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let shapes = published_plan_with_shape(&repo, &provider, &scope, tenant, plan_id).await;

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_11), at(12)),
        )
        .await
        .expect("open the successor revision");
    assert_eq!(opened.revision, 1);

    // All three tables, in one case, because "the shape travels" is one fact
    // about a revision rather than three about three tables: a successor that
    // carried its phases and lost its descriptors is a plan whose next publish
    // is blocked by `DESCRIPTOR_INCOMPLETE` for a set its author never removed.
    assert_eq!(
        shapes
            .list_phases(&scope, tenant, plan_id, 1)
            .await
            .expect("read the successor's phases"),
        three_phases(),
        "the phase chain travels, phase ids and all (D-83/D-56)"
    );
    assert_eq!(
        shapes
            .list_composites(&scope, tenant, plan_id, 1)
            .await
            .expect("read the successor's composites"),
        two_composites(),
        "the composite definitions travel with a stable `composite_id` (D-106), so a formula \
         edit on this draft leaves the published revision byte-identical"
    );
    assert_eq!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 1)
            .await
            .expect("read the successor's add-on rules"),
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 0)
            .await
            .expect("read the predecessor's add-on rules"),
        "the add-on set travels, every rule of it (D-105)"
    );
    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 1)
            .await
            .expect("read the successor's descriptor set"),
        Some(descriptors()),
        "the descriptor set travels, the P5 extra fields included"
    );
    // D-319's bounds. A successor that lost them republishes a plan with no
    // minimum — a price change nobody authored and no line of the invoice
    // explains.
    assert_eq!(
        shapes
            .list_period_floor_caps(&scope, tenant, plan_id, 1)
            .await
            .expect("read the successor's period bounds"),
        two_period_bounds(),
        "the period floor/cap set travels, every market of it (D-319)"
    );

    // Slice 8's three, on the same terms (D-92): a successor that lost its
    // components composes with fewer products than its predecessor at an
    // unchanged price, and one that lost its rev-share pays no vendor at all.
    assert_eq!(
        component_rows(&provider, &scope, plan_id, 1)
            .await
            .iter()
            .map(|row| {
                (
                    row.component_plan_id,
                    row.included_sku_id,
                    row.min_qty,
                    row.max_qty,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            (
                Uuid::from_u128(0x0c01),
                Uuid::from_u128(0x05c1),
                Some(1_i32),
                Some(4_i32)
            ),
            (
                Uuid::from_u128(0x0c02),
                Uuid::from_u128(0x05c2),
                Some(1_i32),
                Some(4_i32)
            ),
        ],
        "the component set travels, every member and every column of it (D-92/D-105): a copy \
         that carried two rows with a defaulted `included_sku_id` or lost the qty bounds \
         composes the successor with different products at an unchanged price"
    );
    let carried_groups = group_rows(&provider, &scope, plan_id, 1).await;
    assert_eq!(
        carried_groups
            .iter()
            .map(|row| {
                (
                    row.vendor_sku_id,
                    row.platform_cut_bp,
                    row.residual_absorber_party.clone(),
                )
            })
            .collect::<Vec<_>>(),
        vec![(
            Uuid::from_u128(0x00be_110d),
            1000_i32,
            "platform".to_owned()
        )],
        "the rev-share group travels with its platform cut and its named absorber: a defaulted \
         `platform_cut_bp` republishes the bundle with a different platform take"
    );
    let carried_parties = party_rows(&provider, &scope, plan_id, 1).await;
    assert_eq!(carried_parties.len(), 2, "every party of the group travels");
    // The typed share travels; the **effective** share does not, because it is
    // the previous publish's normalization and the successor has not published.
    // A draft carrying the predecessor's effective shares would reconcile the
    // old split the moment its typed shares were edited.
    assert!(
        carried_parties
            .iter()
            .all(|party| party.share_bp == 4500 && party.effective_share_bp.is_none()),
        "the typed share travels and the normalized one does not (D-07), got: {carried_parties:?}"
    );

    // The ids are the load-bearing half of the phase copy: the `phase` axis of
    // the canonical scope key holds a bare `phase_id` (D-19) and same-key
    // supersession compares it (D-56), so a re-minted one would move every
    // continuing price row onto a key nothing is filed under.
    assert_eq!(
        shapes
            .list_phases(&scope, tenant, plan_id, 1)
            .await
            .expect("read")
            .iter()
            .map(|phase| phase.phase_id)
            .collect::<Vec<PhaseId>>(),
        three_phases()
            .iter()
            .map(|phase| phase.phase_id)
            .collect::<Vec<PhaseId>>(),
    );

    // And the published revision keeps all three of its own: a copy is a copy,
    // and the frozen revision's shape is what the projector re-reads at every
    // warm re-drive.
    assert_eq!(
        shapes
            .list_phases(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        three_phases()
    );
    assert_eq!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 0)
            .await
            .expect("read")
            .len(),
        3
    );
    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        Some(descriptors())
    );
}

/// The other half of the retired anchor: a discarded revision keeps **none** of
/// its copies, and its predecessor keeps all of them.
///
/// The drop precedes the flip inside one transaction, and that ordering is
/// forced rather than chosen: `abandoned` is not `draft`, so every child table's
/// DELETE trigger refuses the drop the moment the revision row has moved. A
/// tombstone that kept its copies would leave a frozen shape hanging off a
/// revision no path can reach, under a number that stays consumed forever.
#[tokio::test]
async fn an_abandoned_revision_keeps_none_of_the_whole_shape_d145() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let shapes = published_plan_with_shape(&repo, &provider, &scope, tenant, plan_id).await;

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_11), at(12)),
        )
        .await
        .expect("open the successor revision");
    let tombstone = repo
        .abandon_draft(&scope, tenant, plan_id, 1, opened.row_version, stamp())
        .await
        .expect("abandon the draft revision");
    assert_eq!(tombstone.lifecycle_state, LifecycleState::Abandoned);

    assert!(
        shapes
            .list_phases(&scope, tenant, plan_id, 1)
            .await
            .expect("read")
            .is_empty(),
        "no phase rows survive the tombstone"
    );
    assert!(
        shapes
            .list_composites(&scope, tenant, plan_id, 1)
            .await
            .expect("read")
            .is_empty(),
        "no composite rows survive the tombstone: `abandoned` is not `draft`, so the drop has \
         to precede the flip or the table's DELETE trigger refuses it forever"
    );
    assert!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 1)
            .await
            .expect("read")
            .is_empty(),
        "no add-on rules survive the tombstone"
    );
    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 1)
            .await
            .expect("read"),
        None,
        "no descriptor set survives the tombstone"
    );
    assert!(
        shapes
            .list_period_floor_caps(&scope, tenant, plan_id, 1)
            .await
            .expect("read")
            .is_empty(),
        "no period bound survives the tombstone (D-319): `abandoned` is not `draft`, so the \
         drop has to precede the flip or the table's DELETE trigger refuses it forever"
    );

    // Slice 8's three, on the same terms: a tombstone that kept its composition
    // would leave a frozen component set hanging off a revision no path reaches,
    // under a number that stays consumed forever (D-145).
    assert!(
        component_rows(&provider, &scope, plan_id, 1)
            .await
            .is_empty(),
        "no component survives the tombstone"
    );
    assert!(
        group_rows(&provider, &scope, plan_id, 1).await.is_empty(),
        "no rev-share group survives the tombstone"
    );
    assert!(
        party_rows(&provider, &scope, plan_id, 1).await.is_empty(),
        "no rev-share party survives the tombstone"
    );

    // The published revision is untouched by any of it — the drop is scoped to
    // the discarded revision, not to the plan.
    assert_eq!(
        shapes
            .list_phases(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        three_phases()
    );
    assert_eq!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 0)
            .await
            .expect("read")
            .len(),
        3
    );
    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        Some(descriptors())
    );
}

/// **The set of revision-scoped child tables is closed, and every member of it
/// is copied on `open_revision` and dropped on `abandon_draft`.**
///
/// This is the successor to the anchor that stood in this file while the three
/// tables were landing. That one named the tables that did **not** exist yet and
/// failed the moment one appeared; it served its purpose and then ran out of
/// absentees to name. This one names the property instead, so it keeps working
/// with no absentees left: a fourth revision-scoped table turns it red the
/// moment it is created, which is precisely when its author needs to be told
/// that D-83 and D-145 are now partly its problem.
///
/// **`plan_revision` is the discriminator**, and it is a sound one here: the
/// three child tables carry that column and nothing else in the chain does —
/// `pricing_plan` names its own `revision`, and no other table in this gear has a
/// revision column at all. The column is what makes a table revision-scoped in
/// the first place, so the query asks the schema the same question the decision
/// asks: which rows version with a plan revision? A test listing table names
/// would go stale the way the old anchor did; this one cannot, because the thing
/// it enumerates is the thing that creates the obligation.
///
/// The **copy and the drop** are asserted by
/// [`a_new_revision_carries_the_whole_shape_forward_with_stable_ids_d83`] and
/// [`an_abandoned_revision_keeps_none_of_the_whole_shape_d145`] over all three
/// tables at once, and by a per-table case for each. This test is what points a
/// fourth table's author at them.
#[tokio::test]
async fn the_revision_scoped_tables_are_a_closed_set_and_each_one_is_copied_and_dropped() {
    /// Alphabetical, because that is the order the query returns.
    ///
    /// Slice 8's three joined Slice 2's three: `pricing_bundle_component`,
    /// `pricing_bundle_revshare_group` and `pricing_bundle_revshare` version with
    /// the plan revision under D-92, and their copier and dropper live in
    /// `repo/bundle_repo.rs` rather than in `plan_shape_repo.rs` — they hang off
    /// the plan through `pricing_bundle` rather than carrying `plan_id`
    /// themselves, so their statements need that indirection and Slice 2's do
    /// not. `PlanRepo` calls both unconditionally; a plan that is not a bundle is
    /// a no-op.
    const REVISION_SCOPED: [&str; 8] = [
        "pricing_bundle_component",
        "pricing_bundle_revshare",
        "pricing_bundle_revshare_group",
        // Slice 10's composite meters (2026-08-08). Added **after** the obligation
        // this assertion states was met, not to silence it: `copy_composites` and
        // `delete_composites` live in `plan_shape_repo` beside the phase set's,
        // `open_revision` and `abandon_draft` call them in the required order, and
        // the two shape cases below assert both.
        "pricing_composite_meter",
        "pricing_plan_addon_rule",
        "pricing_plan_descriptor_set",
        // D-319's period floor/cap (2026-08-15). Added after the obligation was
        // met, like `pricing_composite_meter` above: `copy_period_floor_caps`
        // and `delete_period_floor_caps` live in `plan_shape_repo` beside the
        // phase set's, `open_revision` and `abandon_draft` call them in the
        // required order, and the two shape cases below assert both.
        "pricing_plan_period_floor_cap",
        "pricing_plan_phase",
    ];

    let conn = common::migrated_db().await;
    // Every table of the whole chain that carries a `plan_revision` column,
    // asked of `sqlite_master` and `pragma_table_info` rather than of a list
    // this file maintains.
    let found = common::scalar(
        &conn,
        "SELECT coalesce(group_concat(name, ','), '') AS v FROM (
           SELECT m.name AS name
             FROM sqlite_master m
             JOIN pragma_table_info(m.name) c
            WHERE m.type = 'table' AND c.name = 'plan_revision'
            ORDER BY m.name)",
    )
    .await;

    assert_eq!(
        found,
        REVISION_SCOPED.join(","),
        "the set of tables carrying `plan_revision` has changed. A revision-scoped \
         table is one whose rows version with a plan revision, and every one of \
         them owes two things that nothing else in this gear will supply: \
         `PlanRepo::open_revision` must copy its rows onto the newly opened \
         revision, inside that method's transaction and after the revision row is \
         inserted (the table's INSERT trigger requires the new parent to be \
         `draft`); and `PlanRepo::abandon_draft` must delete them BEFORE it flips \
         the revision row, because `abandoned` is not `draft` and the DELETE \
         trigger refuses everything afterwards. Add both calls in \
         `src/infra/storage/repo/plan_repo.rs`, add the copier and the dropper \
         beside the table's other statements in `plan_shape_repo.rs`, extend \
         `a_new_revision_carries_the_whole_shape_forward_with_stable_ids_d83` and \
         `an_abandoned_revision_keeps_none_of_the_whole_shape_d145` to assert \
         them, and - the third path, which this message omitted until \
         2026-08-15 - `infra::clone::clone_plan_on` must copy the set onto a \
         cloned plan, which no assertion here can see because a clone is a \
         different plan. Only then add the table here. Do NOT simply add it \
         here: this assertion is the notice, not the obligation."
    );
}

// ---------------------------------------------------------------------------
// D-90's flip: the publish unit's two lifecycle moves.
// ---------------------------------------------------------------------------

/// Count the plan's revisions in one lifecycle state.
async fn count_in_state(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    plan_id: PlanId,
    state: LifecycleState,
) -> u64 {
    let conn = provider.conn().expect("conn");
    plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::LifecycleState.eq(state.as_str())),
        )
        .count(&conn)
        .await
        .expect("count revisions")
}

/// `publish_revision` through a real transaction, which is now what its type
/// demands.
///
/// It takes a `DbTx` rather than any runner because it performs **two**
/// statements that must not be separable: on a bare connection a
/// compare-and-swap failing after the demotion would leave the plan with no
/// current revision at all. These tests therefore drive it the way the publish
/// commit does.
async fn publish_revision(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
) -> Result<bss_pricing::domain::plan::PlanRevision, RepoError> {
    let scope = scope.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<bss_pricing::domain::plan::PlanRevision, RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::plan_repo::publish_revision(
                    txn, &scope, tenant_id, plan_id, revision, expected,
                )
                .await
            })
        })
        .await;
    outcome.map_err(|err| {
        err.into_domain(|infra| RepoError::Db(format!("publish transaction: {infra}")))
    })
}

#[tokio::test]
async fn a_first_publish_leaves_exactly_one_current_revision() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");

    let published = publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        created.row_version,
    )
    .await
    .expect("publish the first revision");

    assert_eq!(published.lifecycle_state, LifecycleState::Published);
    // The tag advances with the flip: the representation a caller cached did
    // change, and this is the last move it will ever make.
    assert_eq!(published.row_version, RowVersion::new(1));
    assert_eq!(
        count_in_state(&provider, &scope, plan_id, LifecycleState::Published).await,
        1
    );
    assert_eq!(
        count_in_state(&provider, &scope, plan_id, LifecycleState::Superseded).await,
        0,
        "a first publish has no predecessor to demote"
    );
    assert_eq!(
        repo.find_current(&scope, tenant, plan_id)
            .await
            .expect("read the current revision")
            .expect("the plan has one")
            .revision,
        created.revision
    );
}

#[tokio::test]
async fn a_second_publish_demotes_the_first_and_still_leaves_exactly_one() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");
    let first = publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        created.row_version,
    )
    .await
    .expect("publish revision 0");

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_11), at(12)),
        )
        .await
        .expect("open the successor");
    publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        opened.revision,
        opened.row_version,
    )
    .await
    .expect("publish the successor");

    assert_eq!(
        count_in_state(&provider, &scope, plan_id, LifecycleState::Published).await,
        1,
        "uq_pricing_plan_current permits exactly one, and the demotion is what keeps it true"
    );
    assert_eq!(
        count_in_state(&provider, &scope, plan_id, LifecycleState::Superseded).await,
        1
    );
    assert_eq!(
        repo.find_current(&scope, tenant, plan_id)
            .await
            .expect("read the current revision")
            .expect("the plan has one")
            .revision,
        opened.revision
    );

    // The demoted predecessor keeps the entity tag it published under: the
    // frozen-column guard lists `row_version`, so a demotion that bumped it
    // would be refused by the trigger, and the tag freezes with the content it
    // names.
    let demoted = repo
        .find_revision(&scope, tenant, plan_id, created.revision)
        .await
        .expect("read the predecessor")
        .expect("it is still there");
    assert_eq!(demoted.lifecycle_state, LifecycleState::Superseded);
    assert_eq!(demoted.row_version, first.row_version);
}

#[tokio::test]
async fn a_stale_row_version_is_refused_and_nothing_flips() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");

    let refusal = publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        RowVersion::new(99),
    )
    .await
    .expect_err("a stale version must be refused");

    assert!(
        matches!(refusal, RepoError::StaleRowVersion { current, submitted, .. }
            if current == 0 && submitted == 99),
        "got {refusal:?}"
    );
    assert_eq!(
        count_in_state(&provider, &scope, plan_id, LifecycleState::Published).await,
        0,
        "the refused publish left the revision where it was"
    );
    assert_eq!(
        repo.find_revision(&scope, tenant, plan_id, created.revision)
            .await
            .expect("read it back")
            .expect("it is still there")
            .lifecycle_state,
        LifecycleState::Draft
    );
}

#[tokio::test]
async fn publishing_a_revision_twice_is_refused_by_the_state_machine_not_by_the_trigger() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");
    let published = publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        created.row_version,
    )
    .await
    .expect("publish it once");

    // A revision publishes at most once, which is what makes the outbox dedup
    // key `(event, plan, revision)` sound.
    let refusal = publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        published.row_version,
    )
    .await
    .expect_err("a second publish of one revision must be refused");

    assert!(
        matches!(&refusal, RepoError::NotDraft { state, .. } if state == "published"),
        "got {refusal:?}"
    );
}

#[tokio::test]
async fn a_retired_plan_takes_no_publish_and_says_so_in_its_own_words() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");
    let conn = provider.conn().expect("conn");
    publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        created.row_version,
    )
    .await
    .expect("publish revision 0");
    // Retirement is Slice 11's publish unit and has no producer here.
    flip_state(
        &provider,
        &scope,
        plan_id,
        i64::try_from(created.revision).expect("a small revision"),
        LifecycleState::Retired,
    )
    .await;

    // A second revision row, fabricated straight at the table: `open_revision`
    // refuses a retired plan first, and what is under test is the publish.
    let opened = plan::ActiveModel {
        plan_name: Set(None),
        cloned_from: Set(None),
        entitlement_grants: Set(None),
        allowed_change_targets: Set(None),
        comparability_rank: Set(None),
        usage_counter_on_plan_change: Set(None),
        plan_id: sea_orm::ActiveValue::Set(plan_id.get()),
        revision: sea_orm::ActiveValue::Set(1),
        tenant_id: sea_orm::ActiveValue::Set(tenant),
        sku_id: sea_orm::ActiveValue::Set(None),
        plan_tier: sea_orm::ActiveValue::Set(None),
        billing_cycle: sea_orm::ActiveValue::Set(None),
        frequency: sea_orm::ActiveValue::Set(None),
        custom_interval_n: sea_orm::ActiveValue::Set(None),
        custom_interval_unit: sea_orm::ActiveValue::Set(None),
        plan_tier_override: sea_orm::ActiveValue::Set(false),
        purchase_min_qty: sea_orm::ActiveValue::Set(None),
        purchase_max_qty: sea_orm::ActiveValue::Set(None),
        invoice_grouping_key: sea_orm::ActiveValue::Set(None),
        lifecycle_state: sea_orm::ActiveValue::Set(LifecycleState::Draft.as_str().to_owned()),
        available_from: sea_orm::ActiveValue::Set(None),
        available_to: sea_orm::ActiveValue::Set(None),
        created_by: sea_orm::ActiveValue::Set(Uuid::from_u128(0xac_11)),
        created_at_utc: sea_orm::ActiveValue::Set(at(12)),
        row_version: sea_orm::ActiveValue::Set(0),
    };
    plan::Entity::insert(opened.clone())
        .secure()
        .scope_with_model(&scope, &opened)
        .expect("scope the fabricated draft")
        .exec(&conn)
        .await
        .expect("insert the fabricated draft");

    let refusal = publish_revision(&provider, &scope, tenant, plan_id, 1, RowVersion::new(0))
        .await
        .expect_err("a retired plan takes no successor");

    assert!(
        matches!(&refusal, RepoError::NoSuccessorRevision { state, .. } if state == "retired"),
        "got {refusal:?}"
    );
    assert!(
        matches!(
            repo_failure(&refusal),
            bss_pricing::domain::error::DomainError::PlanRetiredNoSuccessor(_)
        ),
        "the refusal reaches a consumer as PLAN_RETIRED_NO_SUCCESSOR and not as a lifecycle refusal"
    );
}

#[tokio::test]
async fn the_child_shape_rows_are_untouched_by_the_flip_and_freeze_with_the_revision() {
    let (repo, provider) = harness().await;
    let shapes = PlanShapeRepo::new(provider.clone());
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");
    let terminal = PhaseId::new(Uuid::from_u128(0xfa_5e));
    let after_phases = shapes
        .replace_phases(
            &scope,
            tenant,
            plan_id,
            created.revision,
            created.row_version,
            vec![PlanPhase {
                phase_id: terminal,
                kind: PhaseKind::Evergreen,
                display_name: None,
                ordinal: 0,
                converts_to_phase_id: None,
                phase_duration_days: None,
                display_trial_days: None,
            }],
            stamp(),
        )
        .await
        .expect("attach the phase chain");

    publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        after_phases.row_version,
    )
    .await
    .expect("publish the revision");

    let phases = shapes
        .list_phases(&scope, tenant, plan_id, created.revision)
        .await
        .expect("read the phases back");
    assert_eq!(phases.len(), 1);
    assert_eq!(
        phases[0].phase_id, terminal,
        "the id is stable across the flip"
    );

    // And they are frozen with it: the child table's trigger refuses DML under
    // a parent that is no longer `draft`.
    let refusal = shapes
        .replace_phases(
            &scope,
            tenant,
            plan_id,
            created.revision,
            RowVersion::new(after_phases.row_version.get() + 1),
            Vec::new(),
            stamp(),
        )
        .await
        .expect_err("a published revision's shape is immutable");
    assert!(
        matches!(&refusal, RepoError::NotDraft { state, .. } if state == "published"),
        "got {refusal:?}"
    );
}

/// The premise `publish_revision`'s flip order rests on, asserted against the
/// database rather than against the repository.
///
/// `uq_pricing_plan_current` is partial on `lifecycle_state IN
/// ('published','retired')` and both backends evaluate a unique index **per
/// statement**. So publishing the successor before demoting its predecessor
/// would, for the duration of that one statement, put two rows of one plan
/// inside the predicate — and the index rejects. Demoting first leaves the slot
/// empty. This test is what makes a later "simplification" of that order fail
/// here rather than at somebody's second publish.
#[tokio::test]
async fn two_current_revisions_of_one_plan_are_rejected_by_the_index() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");
    let conn = provider.conn().expect("conn");
    publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        created.row_version,
    )
    .await
    .expect("publish revision 0");

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_11), at(12)),
        )
        .await
        .expect("open the successor");
    // The successor's flip, taken alone and with the predecessor left standing:
    // exactly what the wrong order would issue.
    let refused = plan::Entity::update_many()
        .secure()
        .scope_with(&scope)
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Published.as_str()),
        )
        .filter(
            Condition::all()
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(
                    plan::Column::Revision
                        .eq(i64::try_from(opened.revision).expect("a small revision")),
                ),
        )
        .exec(&conn)
        .await;

    // **Which index refused.** `is_err()` alone is satisfied by a CHECK or by the
    // append-only trigger as readily as by an index, and this case's doc names one
    // mechanism: the partial `uq_pricing_plan_current`, over `plan_id` where
    // `lifecycle_state IN ('published','retired')`.
    //
    // SQLite prints the columns an index spans, never its name, and the three
    // unique indexes here are `pricing_plan_pkey (plan_id, revision)`,
    // `uq_pricing_plan_current (plan_id)` and `uq_pricing_plan_open_draft
    // (plan_id)`. Two of them therefore print the *same* list, so the column list
    // alone cannot name the refuser. It is asserted with the pkey's second column
    // excluded, which separates the pair from the key; what separates the pair
    // from each other is their predicates, and only one of the two can be about
    // this statement: `uq_pricing_plan_open_draft` selects `lifecycle_state =
    // 'draft'` and the statement above sets `published`.
    let message = format!("{refused:?}");
    assert!(
        message.contains("UNIQUE constraint failed: pricing_plan.plan_id"),
        "a unique index over plan_id must refuse a second current revision, got: {message}"
    );
    assert!(
        !message.contains("pricing_plan.revision"),
        "and not `pricing_plan_pkey`, whose list carries `revision` and which a second \
         revision of one plan does not collide on: {message}"
    );
}

/// The postcondition of a refused repeat publish — and an honest note about
/// what does and does not stand behind it.
///
/// The defect this group fixed was that `publish_revision` read the current
/// revision first, so a repeat publish of an already-published revision found
/// **itself** as current, demoted it, and only then discovered it was not a
/// draft — leaving the plan with no current revision at all.
///
/// **This test does not prove that fix, and an earlier version of this comment
/// claimed it did.** Reintroducing the defect leaves this test green. The reason
/// is the other half of the same round of work: `publish_revision` now takes a
/// `DbTx`, so a stray demotion is always rolled back with the refusal and the
/// bad state is not observable through the API at all. The **type** is what
/// excludes it; the `mutable_draft` pre-read is belt to that braces, and what it
/// actually buys is a precise refusal taken before a write is wasted.
///
/// What this test pins is the postcondition itself, which is worth pinning
/// cheaply: a refused publish leaves the plan its current revision and demotes
/// nothing. It would earn its keep again the day something calls
/// `publish_revision` outside a transaction — which the signature now forbids,
/// and which is the point.
#[tokio::test]
async fn a_refused_repeat_publish_leaves_the_plan_its_current_revision() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let created = repo
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");
    let published = publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        created.row_version,
    )
    .await
    .expect("publish it once");

    let _refusal = publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        published.row_version,
    )
    .await
    .expect_err("a second publish of one revision is refused");

    let current = repo
        .find_current(&scope, tenant, plan_id)
        .await
        .expect("read the current revision")
        .expect("the plan must still have one");
    assert_eq!(current.revision, created.revision);
    assert_eq!(current.lifecycle_state, LifecycleState::Published);
    assert_eq!(
        count_in_state(&provider, &scope, plan_id, LifecycleState::Superseded).await,
        0,
        "the refused publish must not have demoted the revision it was refusing to publish"
    );
}

/// The actor and instant every mutating repository call now records (D-135 - the
/// audit row commits inside the mutation's own transaction).
fn stamp() -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: uuid::Uuid::from_u128(0xac_10),
        recorded_at: OffsetDateTime::now_utc(),
        correlation_id: TEST_CORRELATION,
    }
}

// ---------------------------------------------------------------------------
// The audit record is inside the mutation's own transaction (D-135).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_edit_whose_record_cannot_be_written_does_not_land_either() {
    // D-135's clause, from the direction a post-hoc writer fails: the record
    // commits **inside** the mutation's transaction, so an append that refuses
    // takes the edit back with it. A writer that appended after the mutation had
    // committed would leave the edit in place and the trail short - which is the
    // silently-incomplete state that is worse than a visibly absent one, because
    // a reader cannot tell "nobody changed this" from "this path does not write".
    //
    // The append is made to refuse the way `sqlite_publish_commit.rs` does it: a
    // segment head whose `row_hash` is not 32 bytes is an invariant breach the
    // writer refuses rather than pads.
    let (plans, provider) = harness().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::now_v7());
    let created = plans
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");

    // The create's own record sits at seq 0, so the corrupt head goes above it.
    let conn = provider.conn().expect("conn");
    let corrupt = bss_pricing::infra::storage::entity::audit_log::ActiveModel {
        tenant_id: sea_orm::ActiveValue::Set(tenant),
        chain_id: sea_orm::ActiveValue::Set(plan_id.get()),
        seq: sea_orm::ActiveValue::Set(1),
        entry_kind: sea_orm::ActiveValue::Set("mutation".to_owned()),
        recorded_at: sea_orm::ActiveValue::Set(at(9)),
        actor_principal_id: sea_orm::ActiveValue::Set(Uuid::from_u128(0xac_10)),
        action: sea_orm::ActiveValue::Set("update".to_owned()),
        subject_kind: sea_orm::ActiveValue::Set("plan_revision".to_owned()),
        subject_ref: sea_orm::ActiveValue::Set(format!("{plan_id}/0")),
        before_state: sea_orm::ActiveValue::Set(None),
        after_state: sea_orm::ActiveValue::Set(None),
        approval_ref: sea_orm::ActiveValue::Set(None),
        correlation_id: sea_orm::ActiveValue::Set(None),
        segment_heads: sea_orm::ActiveValue::Set(None),
        prev_hash: sea_orm::ActiveValue::Set(None),
        row_hash: sea_orm::ActiveValue::Set(vec![0_u8]),
    };
    bss_pricing::infra::storage::entity::audit_log::Entity::insert(corrupt.clone())
        .secure()
        .scope_with_model(&scope, &corrupt)
        .expect("scope the seeded head")
        .exec(&conn)
        .await
        .expect("seed a head the writer cannot link to");

    let refusal = plans
        .update_draft(
            &scope,
            tenant,
            plan_id,
            0,
            created.row_version,
            PlanShapePatch {
                plan_tier: Some("silver".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect_err("the audit append must refuse");
    assert!(
        matches!(refusal, RepoError::CorruptRow(_)),
        "got {refusal:?}"
    );

    let still = plans
        .find_revision(&scope, tenant, plan_id, 0)
        .await
        .expect("read the revision")
        .expect("it is there");
    assert_eq!(
        still.plan_tier.as_deref(),
        Some("gold"),
        "the edit rolled back with the record that could not be written"
    );
    assert_eq!(
        still.row_version, created.row_version,
        "and so did its tag, so a caller's ETag is still the one the store holds"
    );
}

// ---------------------------------------------------------------------------
// A same-aggregate contention is tellable from a dead connection (D-159)
// ---------------------------------------------------------------------------
//
// `uq_pricing_plan_current` is D-159's third serialization point, and its
// violation is **unprovable in this suite, provably so**: `publish_revision`
// demotes the predecessor and promotes the successor in the *same* transaction
// (that is what its `DbTx` parameter is for), so a single writer never leaves two
// `published` revisions for the index to refuse. Only two concurrent publishes
// can, and `sqlite::memory:` serializes writers.
//
// What is proved is the recognition, which is one function at all three points:
// `tests/sqlite_publish_commit.rs` provokes a real unique violation at the outbox
// and asserts it arrives as `RepoError::ConcurrentMutation` -> 409
// `CONCURRENT_MUTATION`, and `tests/sqlite_audit_chain.rs` asserts the narrowing
// holds - a corrupt row is still a fault. A Postgres suite would add the
// concurrent demonstration at all three and the **constraint names** the driver's
// class does not carry.

#[tokio::test]
async fn a_create_whose_record_cannot_be_written_does_not_land_either() {
    // `PlanRepo::create_draft` used to delegate to `create_draft_on(&self.conn()?,
    // …)`, and `conn()` is documented as the **non-transactional** runner - so the
    // revision insert and its audit append were two autocommit statements. An
    // append failure left a committed revision with no record of who created it,
    // which is exactly the silently-incomplete trail this writer exists to
    // prevent.
    //
    // Delete the `in_transaction` wrapper and this test fails: the revision
    // survives with no record.
    let (plans, provider) = harness().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::now_v7());

    // A head the writer cannot link to, seated at the position revision 0's own
    // record will target.
    let conn = provider.conn().expect("conn");
    let corrupt = bss_pricing::infra::storage::entity::audit_log::ActiveModel {
        tenant_id: sea_orm::ActiveValue::Set(tenant),
        chain_id: sea_orm::ActiveValue::Set(plan_id.get()),
        seq: sea_orm::ActiveValue::Set(0),
        entry_kind: sea_orm::ActiveValue::Set("mutation".to_owned()),
        recorded_at: sea_orm::ActiveValue::Set(at(9)),
        actor_principal_id: sea_orm::ActiveValue::Set(Uuid::from_u128(0xac_10)),
        action: sea_orm::ActiveValue::Set("create".to_owned()),
        subject_kind: sea_orm::ActiveValue::Set("plan_revision".to_owned()),
        subject_ref: sea_orm::ActiveValue::Set(format!("{plan_id}/0")),
        before_state: sea_orm::ActiveValue::Set(None),
        after_state: sea_orm::ActiveValue::Set(None),
        approval_ref: sea_orm::ActiveValue::Set(None),
        correlation_id: sea_orm::ActiveValue::Set(None),
        segment_heads: sea_orm::ActiveValue::Set(None),
        prev_hash: sea_orm::ActiveValue::Set(None),
        row_hash: sea_orm::ActiveValue::Set(vec![0_u8]),
    };
    bss_pricing::infra::storage::entity::audit_log::Entity::insert(corrupt.clone())
        .secure()
        .scope_with_model(&scope, &corrupt)
        .expect("scope the seeded head")
        .exec(&conn)
        .await
        .expect("seed a head the writer cannot link to");

    let refusal = plans
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect_err("the audit append must refuse");
    assert!(
        matches!(refusal, RepoError::CorruptRow(_)),
        "got {refusal:?}"
    );

    assert!(
        plans
            .find_revision(&scope, tenant, plan_id, 0)
            .await
            .expect("read the revision")
            .is_none(),
        "the revision rolled back with the record that could not be written"
    );
}

#[tokio::test]
async fn opening_a_successor_whose_record_cannot_be_written_mints_no_revision_number() {
    // The other half of the same property, on the path an operator reaches through
    // `PATCH /plans/{planId}`. A revision number is permanent (D-145) - every
    // revision-scoped child table copies against it and the audit trail records
    // it - so a number minted with no record of the minting is a name nobody can
    // account for, forever.
    let (plans, provider) = harness().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::now_v7());
    let created = plans
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the draft");
    publish_revision(
        &provider,
        &scope,
        tenant,
        plan_id,
        created.revision,
        created.row_version,
    )
    .await
    .expect("the first revision publishes");

    // The create's record sits at seq 0, so the corrupt head goes above it.
    let conn = provider.conn().expect("conn");
    let corrupt = bss_pricing::infra::storage::entity::audit_log::ActiveModel {
        tenant_id: sea_orm::ActiveValue::Set(tenant),
        chain_id: sea_orm::ActiveValue::Set(plan_id.get()),
        seq: sea_orm::ActiveValue::Set(1),
        entry_kind: sea_orm::ActiveValue::Set("mutation".to_owned()),
        recorded_at: sea_orm::ActiveValue::Set(at(9)),
        actor_principal_id: sea_orm::ActiveValue::Set(Uuid::from_u128(0xac_10)),
        action: sea_orm::ActiveValue::Set("create".to_owned()),
        subject_kind: sea_orm::ActiveValue::Set("plan_revision".to_owned()),
        subject_ref: sea_orm::ActiveValue::Set(format!("{plan_id}/1")),
        before_state: sea_orm::ActiveValue::Set(None),
        after_state: sea_orm::ActiveValue::Set(None),
        approval_ref: sea_orm::ActiveValue::Set(None),
        correlation_id: sea_orm::ActiveValue::Set(None),
        segment_heads: sea_orm::ActiveValue::Set(None),
        prev_hash: sea_orm::ActiveValue::Set(None),
        row_hash: sea_orm::ActiveValue::Set(vec![0_u8]),
    };
    bss_pricing::infra::storage::entity::audit_log::Entity::insert(corrupt.clone())
        .secure()
        .scope_with_model(&scope, &corrupt)
        .expect("scope the seeded head")
        .exec(&conn)
        .await
        .expect("seed a head the writer cannot link to");

    let refusal = plans
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(12)),
        )
        .await
        .expect_err("the audit append must refuse");
    assert!(
        matches!(refusal, RepoError::CorruptRow(_)),
        "got {refusal:?}"
    );

    assert!(
        plans
            .find_revision(&scope, tenant, plan_id, 1)
            .await
            .expect("read the revision")
            .is_none(),
        "no revision number was consumed by a transaction that could not record it"
    );
    assert!(
        plans
            .find_open_draft(&scope, tenant, plan_id)
            .await
            .expect("read the open draft")
            .is_none(),
        "and the plan holds no half-opened successor"
    );
}

/// **Lineage round-trips, and it carries forward to the next revision.**
///
/// `cloned_from` is provenance (D-264), and until the cloner lands nothing in
/// production writes it — so without this case the column would be `NULL` in
/// every row any test ever produced, and the register's claim that it survives
/// `open_revision` would rest on reading the function rather than running it.
/// That is the shape this program keeps correcting: a claim in prose the code
/// does not demonstrate.
///
/// The carry-forward is the load-bearing half. Lineage is the **plan's** and not
/// one revision's, so a second revision of a cloned plan is still cloned; a
/// version of `open_revision` that dropped it would leave a clone's first
/// re-publish looking authored, and nothing else would notice.
#[tokio::test]
async fn lineage_round_trips_and_survives_the_next_revision() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let source = PlanId::new(Uuid::from_u128(0x9_1a5));

    let created = repo
        .create_draft(
            &scope,
            NewPlanDraft {
                cloned_from: Some(source),
                ..new_draft(plan_id, tenant)
            },
        )
        .await
        .expect("create the cloned draft");
    assert_eq!(
        created.cloned_from,
        Some(source),
        "the create must return the lineage it was given"
    );

    let read_back = repo
        .find_revision(&scope, tenant, plan_id, 0)
        .await
        .expect("read it back")
        .expect("the revision is there");
    assert_eq!(
        read_back.cloned_from,
        Some(source),
        "and it must survive the round trip through storage"
    );

    flip_state(&provider, &scope, plan_id, 0, LifecycleState::Published).await;
    let next = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_20), at(12)),
        )
        .await
        .expect("open a successor");
    assert_eq!(
        next.cloned_from,
        Some(source),
        "lineage is the plan's, not one revision's, so the successor is still \
         cloned from the same source"
    );

    // And an authored plan stays authored, so the assertions above are about the
    // value carried rather than about a column that is always set.
    let authored = PlanId::new(Uuid::from_u128(0x9_1a6));
    let plain = repo
        .create_draft(&scope, new_draft(authored, tenant))
        .await
        .expect("create an authored draft");
    assert_eq!(plain.cloned_from, None, "an authored plan has no lineage");
}

// ---------------------------------------------------------------------------
// `PlanCreated` / `PlanUpdated` — the producers S2 §7 puts on the authoring door
// ---------------------------------------------------------------------------

/// Every outbox row of one tenant, in the order the aggregate holds them, as
/// `(event name, aggregate id, dedup key)`.
///
/// The dedup key is read rather than the payload, because the key is what a
/// second enqueue of one act collides on and it is therefore the half of the
/// record a consumer's at-least-once dedup depends on. Ordered by `seq`, because
/// per-`(tenantId, aggregateId)` order is the delivery contract and a suite that
/// read the rows unordered could not tell a creation announced before its edits
/// from one announced after.
async fn plan_outbox(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
) -> Vec<(String, Uuid, String)> {
    let conn = provider.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(scope)
        .order_by(outbox::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the outbox")
        .into_iter()
        .map(|row| (row.event_name, row.aggregate_id, row.dedup_key))
        .collect()
}

#[tokio::test]
async fn creating_a_plan_emits_plan_created() {
    // `inst-pa-return` states the producer in as many words - "RETURN 201 (draft
    // plan, ETag); `PlanCreated` emitted by the Foundation outbox" - and PRD §4
    // makes the emission a MUST rather than one of the conditional names. The
    // name has been frozen and producerless since the gear was created, so every
    // consumer counting plan creations has been counting zero while the exactly
    // parallel `PriceCreated` fired.
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create the first revision");

    assert_eq!(
        plan_outbox(&provider, &scope).await,
        vec![(
            "PlanCreated".to_owned(),
            plan_id.get(),
            format!("PlanCreated/{plan_id}")
        )],
        "one creation, one event, ordered within the plan's own stream"
    );
}

#[tokio::test]
async fn each_authoring_edit_emits_one_plan_updated() {
    // S2 §7: this slice "rides the Foundation's frozen set - `PlanCreated`,
    // `PlanUpdated` on authoring", and S10 §7 adds that the advanced primitives
    // "ride `PlanUpdated`/`PriceUpdated`" rather than minting names of their
    // own. Those primitives are authored as `PATCH` facets, so a facet write
    // that announced nothing would leave S10's whole event story unproduced.
    // PRD AC #98 fixes the granularity: emitted "after mutation".
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let shapes = PlanShapeRepo::new(provider.clone());

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    // The plan facet, then a child-set facet: the two are written by different
    // repositories and share only the audit rail, so a producer bolted onto
    // `update_draft_on` alone would leave the phase write silent.
    repo.update_draft(
        &scope,
        tenant,
        plan_id,
        0,
        RowVersion::new(0),
        PlanShapePatch {
            plan_tier: Some("platinum".to_owned()),
            ..PlanShapePatch::default()
        },
        stamp(),
    )
    .await
    .expect("edit the plan facet");
    shapes
        .replace_phases(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(1),
            three_phases(),
            stamp(),
        )
        .await
        .expect("author the phase chain");

    assert_eq!(
        plan_outbox(&provider, &scope).await,
        vec![
            (
                "PlanCreated".to_owned(),
                plan_id.get(),
                format!("PlanCreated/{plan_id}")
            ),
            // The post-state row version is what names the edit: the
            // compare-and-swap makes `(plan, revision, version)` unique to one
            // mutation, so two edits can never collide on one key and a
            // redelivery of one edit can never look like two.
            (
                "PlanUpdated".to_owned(),
                plan_id.get(),
                format!("PlanUpdated/{plan_id}/0/1")
            ),
            (
                "PlanUpdated".to_owned(),
                plan_id.get(),
                format!("PlanUpdated/{plan_id}/0/2")
            ),
        ],
        "one event per authoring mutation, each naming the state it produced"
    );
}

#[tokio::test]
async fn abandoning_a_draft_emits_nothing_further() {
    // The discriminator is the act, not the rail. `abandon_draft` writes its
    // audit record through the same function every facet edit does, so a
    // producer placed there without asking what happened would announce a
    // discarded draft as a content change. D-145 is explicit that the abandon is
    // a terminal lifecycle flip rather than an edit, and the frozen set has no
    // name for it - so the honest wire outcome is silence.
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("create");
    repo.abandon_draft(&scope, tenant, plan_id, 0, RowVersion::new(0), stamp())
        .await
        .expect("abandon the draft");

    assert_eq!(
        plan_outbox(&provider, &scope)
            .await
            .into_iter()
            .map(|(name, _, _)| name)
            .collect::<Vec<_>>(),
        vec!["PlanCreated".to_owned()],
        "the creation is announced; the abandon is not"
    );
}

// ---------------------------------------------------------------------------
// D-360: the tenant's plan counts under the list's authoring collapse.
// ---------------------------------------------------------------------------

/// Two plans holding only a draft, one published, one retired, and one
/// published plan with an **open successor draft**: the last one counts as
/// `draft`, because that is the row the list shows for it. `total` is the
/// number of plans, not of rows - the successor plan holds two.
#[tokio::test]
async fn the_authoring_state_counts_fold_a_successor_draft_onto_its_plan() {
    let (repo, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let ids: Vec<PlanId> = (1..=5_u128)
        .map(|n| PlanId::new(Uuid::from_u128(0xd3_60 + n)))
        .collect();
    for id in &ids {
        repo.create_draft(&scope, new_draft(*id, tenant))
            .await
            .expect("create the draft");
    }
    // ids[0], ids[1]: drafts. ids[2]: published. ids[3]: retired.
    flip_state(&provider, &scope, ids[2], 0, LifecycleState::Published).await;
    flip_state(&provider, &scope, ids[3], 0, LifecycleState::Published).await;
    flip_state(&provider, &scope, ids[3], 0, LifecycleState::Retired).await;
    // ids[4]: published, then a successor draft opened over it.
    flip_state(&provider, &scope, ids[4], 0, LifecycleState::Published).await;
    let successor = repo
        .open_revision(
            &scope,
            tenant,
            ids[4],
            stamp_of(Uuid::from_u128(0xac_20), at(12)),
        )
        .await
        .expect("the successor opens");
    assert_eq!(successor.revision, 1, "the premise: two rows for one plan");

    let conn = provider.conn().expect("conn");
    let counts = count_plans_by_authoring_state(&conn, &scope, tenant)
        .await
        .expect("count");

    assert_eq!(
        counts,
        PlanCounts {
            total: 5,
            draft: 3,
            published: 1,
            retired: 1,
        },
        "the successor-draft plan is draft, as the list shows it"
    );
    assert_eq!(
        count_plans_by_authoring_state(
            &conn,
            &AccessScope::for_tenant(Uuid::from_u128(0x99)),
            Uuid::from_u128(0x99)
        )
        .await
        .expect("count another tenant"),
        PlanCounts::default(),
        "another tenant's catalogue counts nothing here"
    );
}
