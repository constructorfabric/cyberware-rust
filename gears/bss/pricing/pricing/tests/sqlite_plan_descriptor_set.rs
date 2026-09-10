//! `pricing_plan_descriptor_set` against a real database — the 1:1 key, the
//! nullability that makes `DESCRIPTOR_INCOMPLETE` reachable, the append-only
//! triggers, and the upsert that writes the row.
//!
//! Everything worth proving here is a property of the **schema**, of a
//! **statement**, or of what the repository leaves behind — never of a branch in
//! Rust. A mock would assert that the repository's own `if` fires and would keep
//! asserting it after the predicate that matters had been deleted.
//!
//! The suite carries **two** harnesses, as its two sibling suites do. The
//! constraint and trigger cases drive raw SQL through `common::migrated_db`,
//! because the states they need — a descriptor row under a published revision,
//! one under an abandoned revision — are states no typed path will ever produce
//! and the guards exist precisely for what reaches the table outside this gear.
//! The repository cases drive `PlanShapeRepo`.
//!
//! Postgres mirrors every rule here as one PL/pgSQL trigger function and the
//! same index, FK and PK, so no testcontainers case is added; see the
//! migration's module doc for the transform.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;

use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::plan_shape::DescriptorSet;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::plan;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{NewPlanDraft, PlanRepo, PlanShapeRepo};

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

mod common;

use common::{exec, migrated_db, must_succeed, scalar};

/// One value for a whole test binary: these suites drive a repository or a
/// service directly, where the value the HTTP edge would have established has
/// no producer. What each suite asserts *about* it is stated where it asserts
/// it.
const TEST_CORRELATION: uuid::Uuid = uuid::Uuid::from_u128(0x_c0_11_a7_10);

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const PLAN: &str = "22222222-2222-2222-2222-222222222222";
const ACTOR: &str = "44444444-4444-4444-4444-444444444444";
const AUTHORED: &str = "2026-08-02 10:00:00 +00:00";

/// Reject, **and** for the stated reason.
///
/// The fragment is not decoration: this table carries a primary key, a composite
/// foreign key and three append-only triggers, and a test that accepted any
/// error naming the table would pass with the guard it means to prove switched
/// off, refused instead by a constraint it never intended to trip.
async fn must_be_rejected(conn: &DatabaseConnection, sql: &str, because: &str) {
    let err = exec(conn, sql)
        .await
        .expect_err(&format!("this statement must be rejected: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains("pricing_plan_descriptor_set"),
        "the rejection must name the guard it came from, got: {message}"
    );
    assert!(
        message.contains(because),
        "the rejection must be the one under test (`{because}`), got: {message}"
    );
}

async fn insert_revision(conn: &DatabaseConnection, revision: i64) {
    must_succeed(
        conn,
        &format!(
            "INSERT INTO pricing_plan (
                plan_id, revision, tenant_id, lifecycle_state, created_by, created_at_utc)
             VALUES ('{PLAN}', {revision}, '{TENANT}', 'draft', '{ACTOR}', '{AUTHORED}')"
        ),
    )
    .await;
}

/// Move a revision out of `draft`. Both flips this suite uses —
/// `draft -> published` and `draft -> abandoned` — are whitelisted by
/// `pricing_plan`'s own append-only trigger.
async fn flip(conn: &DatabaseConnection, revision: i64, state: &str) {
    must_succeed(
        conn,
        &format!(
            "UPDATE pricing_plan SET lifecycle_state = '{state}' \
             WHERE plan_id = '{PLAN}' AND revision = {revision}"
        ),
    )
    .await;
}

fn insert_descriptors(revision: i64, gl_code: &str) -> String {
    format!(
        "INSERT INTO pricing_plan_descriptor_set (
            plan_id, plan_revision, tenant_id, invoice_line_template, gl_code, itemization_rule)
         VALUES ('{PLAN}', {revision}, '{TENANT}', 'Subscription: {{plan}}', '{gl_code}', 'per_plan')"
    )
}

async fn count_sets(conn: &DatabaseConnection, revision: i64) -> String {
    scalar(
        conn,
        &format!(
            "SELECT CAST(count(*) AS TEXT) AS v FROM pricing_plan_descriptor_set \
             WHERE plan_id = '{PLAN}' AND plan_revision = {revision}"
        ),
    )
    .await
}

// ---------------------------------------------------------------------------
// The key and the columns.
// ---------------------------------------------------------------------------

/// One set per revision, and one per revision **of each revision**.
#[tokio::test]
async fn a_revision_holds_exactly_one_descriptor_set() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;
    must_succeed(&conn, &insert_descriptors(0, "4000")).await;

    // The 1:1 key is what §6 says this table has, and it is why the repository's
    // "set" and "replace" are one call: a second row for one revision is not a
    // second descriptor set, it is an ambiguity about which one Billing posts
    // from.
    must_be_rejected(
        &conn,
        &insert_descriptors(0, "4001"),
        "UNIQUE constraint failed",
    )
    .await;
    assert_eq!(count_sets(&conn, 0).await, "1");

    // A different revision of the same plan carries its own, which is the
    // copy-on-new-revision shape (D-83): the key is the plan **and** the
    // revision, so the successor's copy is a distinct row.
    flip(&conn, 0, "published").await;
    insert_revision(&conn, 1).await;
    must_succeed(&conn, &insert_descriptors(1, "4001")).await;
    assert_eq!(count_sets(&conn, 0).await, "1");
    assert_eq!(count_sets(&conn, 1).await, "1");
}

/// Every descriptor column is nullable, and that is load-bearing twice over: a
/// draft is authored incrementally (`flow-plan-author` step 4), and a column
/// that cannot be missing is an element `DESCRIPTOR_INCOMPLETE` can never name.
#[tokio::test]
async fn a_half_authored_descriptor_set_is_storable() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_succeed(
        &conn,
        &format!(
            "INSERT INTO pricing_plan_descriptor_set (plan_id, plan_revision, tenant_id, gl_code)
             VALUES ('{PLAN}', 0, '{TENANT}', '4000')"
        ),
    )
    .await;
    let stored = scalar(
        &conn,
        &format!(
            "SELECT coalesce(invoice_line_template, '(none)') || '/' || additional_fields AS v
               FROM pricing_plan_descriptor_set WHERE plan_id = '{PLAN}' AND plan_revision = 0"
        ),
    )
    .await;
    // The extra-field registry defaults to the empty object rather than NULL: an
    // empty object is what "no extra fields" is, and a nullable column would
    // give that state two spellings.
    assert_eq!(stored, "(none)/{}");

    must_be_rejected(
        &conn,
        &format!(
            "UPDATE pricing_plan_descriptor_set SET additional_fields = NULL \
             WHERE plan_id = '{PLAN}' AND plan_revision = 0"
        ),
        "NOT NULL constraint failed",
    )
    .await;
}

#[tokio::test]
async fn a_descriptor_set_belongs_to_a_revision_that_exists() {
    let conn = migrated_db().await;

    // No revision at all: the row is refused. The **append-only trigger**
    // answers rather than the foreign key — its predicate ("a draft revision
    // with this key exists") strictly implies the key's, so on the insert path
    // the trigger can never be reached second.
    must_be_rejected(
        &conn,
        &insert_descriptors(0, "4000"),
        "under a non-draft plan revision is not permitted",
    )
    .await;

    let declared = scalar(
        &conn,
        "SELECT group_concat(\"from\" || '->' || \"to\", ',') AS v
           FROM pragma_foreign_key_list('pricing_plan_descriptor_set')
          WHERE \"table\" = 'pricing_plan'",
    )
    .await;
    assert!(
        declared.contains("plan_id->plan_id") && declared.contains("plan_revision->revision"),
        "the composite FK must cover both key columns, got: {declared}"
    );
}

// ---------------------------------------------------------------------------
// The append-only triggers.
// ---------------------------------------------------------------------------

/// Revision 0 published with a set, revision 2 abandoned with a set, revision 1
/// left open as the plan's one draft.
async fn seeded_planes(conn: &DatabaseConnection) {
    insert_revision(conn, 0).await;
    must_succeed(conn, &insert_descriptors(0, "4000")).await;
    flip(conn, 0, "published").await;

    insert_revision(conn, 2).await;
    must_succeed(conn, &insert_descriptors(2, "4002")).await;
    flip(conn, 2, "abandoned").await;

    insert_revision(conn, 1).await;
    must_succeed(conn, &insert_descriptors(1, "4001")).await;
}

#[tokio::test]
async fn a_frozen_revision_takes_no_descriptor_set_and_a_draft_one_does() {
    let conn = migrated_db().await;
    // Revision 0 published and revision 1 abandoned, both without a set, so the
    // INSERT arm is what answers rather than the primary key; revision 2 is the
    // plan's one open draft.
    insert_revision(&conn, 0).await;
    flip(&conn, 0, "published").await;
    insert_revision(&conn, 1).await;
    flip(&conn, 1, "abandoned").await;
    insert_revision(&conn, 2).await;

    // INSERT is guarded because on a 1:1 table it is how a revision that
    // published **without** a descriptor set would acquire one afterwards —
    // behind the back of the publish that refused it.
    must_be_rejected(
        &conn,
        &insert_descriptors(0, "4000"),
        "INSERT of a descriptor set under a non-draft plan revision",
    )
    .await;
    // `abandoned` is not `draft`, which is exactly what makes the drop-then-flip
    // ordering in `PlanRepo::abandon_draft` mandatory rather than stylistic.
    must_be_rejected(
        &conn,
        &insert_descriptors(1, "4001"),
        "INSERT of a descriptor set under a non-draft plan revision",
    )
    .await;
    must_succeed(&conn, &insert_descriptors(2, "4002")).await;

    assert_eq!(count_sets(&conn, 0).await, "0");
    assert_eq!(count_sets(&conn, 1).await, "0");
    assert_eq!(count_sets(&conn, 2).await, "1");
}

#[tokio::test]
async fn a_frozen_revisions_descriptor_set_may_not_be_edited_and_a_drafts_may() {
    let conn = migrated_db().await;
    seeded_planes(&conn).await;

    let edit = |revision: i64| {
        format!(
            "UPDATE pricing_plan_descriptor_set SET gl_code = '9999' \
             WHERE plan_id = '{PLAN}' AND plan_revision = {revision}"
        )
    };
    // Billing posts from the frozen set; an editable one is a GL code that
    // changes under a `CatalogVersion` that already resolved.
    must_be_rejected(
        &conn,
        &edit(0),
        "UPDATE of a descriptor set under a non-draft plan revision",
    )
    .await;
    must_be_rejected(
        &conn,
        &edit(2),
        "UPDATE of a descriptor set under a non-draft plan revision",
    )
    .await;
    must_succeed(&conn, &edit(1)).await;

    let frozen = scalar(
        &conn,
        &format!(
            "SELECT gl_code AS v FROM pricing_plan_descriptor_set \
             WHERE plan_id = '{PLAN}' AND plan_revision = 0"
        ),
    )
    .await;
    assert_eq!(frozen, "4000", "no rejected UPDATE may have landed");
}

#[tokio::test]
async fn a_descriptor_set_may_not_be_re_pointed_at_a_frozen_revision() {
    let conn = migrated_db().await;
    seeded_planes(&conn).await;

    // The NEW parent. Revision 1 is the open draft, so its own freeze cannot be
    // what refuses this — only the revision the row would land under can.
    // Checking it is not belt-and-braces: re-pointing `plan_revision` is how one
    // would otherwise attach a descriptor set to a frozen revision without ever
    // issuing an INSERT.
    must_be_rejected(
        &conn,
        "UPDATE pricing_plan_descriptor_set SET plan_revision = 0 WHERE plan_revision = 1",
        "UPDATE of a descriptor set under a non-draft plan revision",
    )
    .await;

    // The OLD parent, isolated the same way: the destination is the open draft,
    // so only the revision the row is **leaving** can refuse. Its own row is
    // cleared first so the primary key cannot be what answers.
    must_succeed(
        &conn,
        "DELETE FROM pricing_plan_descriptor_set WHERE plan_revision = 1",
    )
    .await;
    must_be_rejected(
        &conn,
        "UPDATE pricing_plan_descriptor_set SET plan_revision = 1 WHERE plan_revision = 0",
        "UPDATE of a descriptor set under a non-draft plan revision",
    )
    .await;

    assert_eq!(count_sets(&conn, 0).await, "1");
    assert_eq!(count_sets(&conn, 1).await, "0");
}

#[tokio::test]
async fn a_frozen_revisions_descriptor_set_may_not_be_deleted_and_a_drafts_may() {
    let conn = migrated_db().await;
    seeded_planes(&conn).await;

    for revision in [0, 2] {
        must_be_rejected(
            &conn,
            &format!("DELETE FROM pricing_plan_descriptor_set WHERE plan_revision = {revision}"),
            "DELETE of a descriptor set under a non-draft plan revision",
        )
        .await;
    }
    must_succeed(
        &conn,
        "DELETE FROM pricing_plan_descriptor_set WHERE plan_revision = 1",
    )
    .await;

    assert_eq!(count_sets(&conn, 0).await, "1");
    assert_eq!(count_sets(&conn, 2).await, "1");
    assert_eq!(count_sets(&conn, 1).await, "0");
}

// ---------------------------------------------------------------------------
// The repository.
// ---------------------------------------------------------------------------

async fn repo_harness() -> (PlanRepo, PlanShapeRepo, DBProvider<DbError>) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    (
        PlanRepo::new(provider.clone()),
        PlanShapeRepo::new(provider.clone()),
        provider,
    )
}

/// Publish a revision straight at the column, which is the same flip
/// `plan_repo::publish_revision` performs; `pricing_plan`'s append-only trigger
/// whitelists it. Fabricated rather than published so this suite stays about
/// the child table.
async fn publish(provider: &DBProvider<DbError>, scope: &AccessScope, plan_id: PlanId) {
    let conn = provider.conn().expect("conn");
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Published.as_str()),
        )
        .filter(
            Condition::all()
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::Revision.eq(0_i64)),
        )
        .exec(&conn)
        .await
        .expect("publish the revision");
    assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 2, hour, 0, 0)
}

fn draft_of(plan_id: PlanId, tenant_id: Uuid) -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id,
        tenant_id,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(10),
        sku_id: None,
        plan_tier: None,
        billing_cycle: None,
        frequency: None,
        plan_tier_override: false,
        purchase_min_qty: None,
        purchase_max_qty: None,
        invoice_grouping_key: None,
        available_from: None,
        available_to: None,
        cloned_from: None,
        correlation_id: TEST_CORRELATION,
    }
}

/// A complete v1 set plus one P5 extra field.
fn descriptors() -> DescriptorSet {
    DescriptorSet {
        invoice_line_template: Some("Subscription: {plan}".to_owned()),
        gl_code: Some("4000".to_owned()),
        itemization_rule: Some("per_plan".to_owned()),
        additional: BTreeMap::from([("costCentre".to_owned(), "emea-ops".to_owned())]),
    }
}

#[tokio::test]
async fn an_attached_set_reads_back_whole_and_an_unattached_one_is_absent() {
    let (repo, shapes, _provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");

    // Absent, not empty. The distinction survives to the authoring surface: an
    // author who has attached nothing is in a different place from one who
    // attached a set and cleared it.
    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        None
    );

    let attached = shapes
        .set_descriptor_set(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            descriptors(),
            stamp(),
        )
        .await
        .expect("attach the descriptor set");
    // The child row has no entity tag of its own: the plan revision's covers it,
    // so attaching descriptors advances the revision's version.
    assert_eq!(attached.row_version, RowVersion::new(1));

    // Field for field, the P5 extra included: a mapping that dropped a column
    // would still round-trip the identity, and the drop would only surface when
    // Billing posted from the frozen set.
    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        Some(descriptors())
    );
}

/// The upsert **replaces**; it does not merge. Both directions are real edits:
/// `flow-plan-author` attaches descriptors incrementally, so a caller that
/// submits a set with one field cleared means it cleared.
#[tokio::test]
async fn a_second_attach_replaces_the_set_rather_than_merging_into_it() {
    let (repo, shapes, _provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    let attached = shapes
        .set_descriptor_set(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            descriptors(),
            stamp(),
        )
        .await
        .expect("attach");

    let corrected = DescriptorSet {
        gl_code: Some("4100".to_owned()),
        ..DescriptorSet::default()
    };
    shapes
        .set_descriptor_set(
            &scope,
            tenant,
            plan_id,
            0,
            attached.row_version,
            corrected.clone(),
            stamp(),
        )
        .await
        .expect("replace");

    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        Some(corrected),
        "a replacement must not leave the previous set's fields standing"
    );
}

/// An empty set is a valid request and it writes a row: refusing it would make
/// the incremental authoring path unusable, and the publish is where
/// `DESCRIPTOR_INCOMPLETE` names what is missing.
#[tokio::test]
async fn an_empty_set_is_attachable_and_reads_back_as_attached() {
    let (repo, shapes, _provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    shapes
        .set_descriptor_set(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            DescriptorSet::default(),
            stamp(),
        )
        .await
        .expect("an empty set is a valid request");

    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        Some(DescriptorSet::default())
    );
}

#[tokio::test]
async fn a_published_revisions_descriptor_set_is_refused_by_name_not_by_trigger() {
    let (repo, shapes, provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    let attached = shapes
        .set_descriptor_set(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            descriptors(),
            stamp(),
        )
        .await
        .expect("attach");
    publish(&provider, &scope, plan_id).await;

    // The table's own trigger would answer with a raw driver error carrying no
    // state and no subject. The read that precedes the delete is what turns it
    // into a refusal a surface can render, and the remedy it implies — open a
    // new revision — is a real one.
    let err = shapes
        .set_descriptor_set(
            &scope,
            tenant,
            plan_id,
            0,
            attached.row_version,
            DescriptorSet::default(),
            stamp(),
        )
        .await
        .expect_err("a frozen revision's descriptors are not editable");
    assert!(
        matches!(err, RepoError::NotDraft { ref state, .. } if state == "published"),
        "got: {err:?}"
    );
    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        Some(descriptors()),
        "the frozen set must be untouched"
    );
}

#[tokio::test]
async fn a_stale_version_attaches_nothing() {
    let (repo, shapes, _provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    shapes
        .set_descriptor_set(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            descriptors(),
            stamp(),
        )
        .await
        .expect("attach");

    let err = shapes
        .set_descriptor_set(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            DescriptorSet::default(),
            stamp(),
        )
        .await
        .expect_err("a shape edit under a superseded tag must be refused");
    assert!(
        matches!(
            err,
            RepoError::StaleRowVersion {
                current: 1,
                submitted: 0,
                ..
            }
        ),
        "got: {err:?}"
    );

    // The delete runs before the swap answers, so the rollback is what keeps
    // this true — not an ordering that avoided touching the table.
    assert_eq!(
        shapes
            .find_descriptor_set(&scope, tenant, plan_id, 0)
            .await
            .expect("read"),
        Some(descriptors()),
        "a refused replacement must leave the previous set intact"
    );
}

#[tokio::test]
async fn another_tenants_descriptor_set_is_invisible() {
    let (repo, shapes, _provider) = repo_harness().await;
    let owner = Uuid::from_u128(0x7e_11);
    let owner_scope = AccessScope::for_tenant(owner);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&owner_scope, draft_of(plan_id, owner))
        .await
        .expect("create");
    shapes
        .set_descriptor_set(
            &owner_scope,
            owner,
            plan_id,
            0,
            RowVersion::new(0),
            descriptors(),
            stamp(),
        )
        .await
        .expect("attach");

    // SQL-level BOLA: a GL code and an invoice template are commercially
    // sensitive, so a foreign scope sees absence rather than a refusal that
    // would confirm the plan exists.
    let intruder = Uuid::from_u128(0x7e_22);
    let intruder_scope = AccessScope::for_tenant(intruder);
    assert_eq!(
        shapes
            .find_descriptor_set(&intruder_scope, intruder, plan_id, 0)
            .await
            .expect("read"),
        None
    );
    // Not even by naming the owner's tenant id: the scope decides, not the
    // argument.
    assert_eq!(
        shapes
            .find_descriptor_set(&intruder_scope, owner, plan_id, 0)
            .await
            .expect("read"),
        None
    );
    assert_eq!(
        shapes
            .find_descriptor_set(&owner_scope, owner, plan_id, 0)
            .await
            .expect("read"),
        Some(descriptors())
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
// The parent revision's tenancy.
// ---------------------------------------------------------------------------

/// A descriptor set belongs to the tenant its own revision belongs to.
///
/// `sqlite_plan_addon_rule`'s twin one table over, and for the same reason: the
/// composite foreign key covers `(plan_id, plan_revision)` alone while the table
/// is scoped by `tenant_id`, so the key admits a row naming somebody else's
/// tenant. The arm existed on both engines and was executed nowhere — pinned only
/// by the two trigger rosters and the two schema goldens, all of which see the
/// guard's text and not whether it still refuses.
///
/// Pinned by the guard's own message fragment: `must_be_rejected` already requires
/// the table name, and every other arm on this table carries it too.
#[tokio::test]
async fn a_descriptor_set_may_not_claim_a_tenant_its_revision_does_not_belong_to() {
    const TENANT_TWO: &str = "11111111-1111-1111-1111-1111111111b2";
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &format!(
            "INSERT INTO pricing_plan_descriptor_set (
                plan_id, plan_revision, tenant_id, invoice_line_template, gl_code,
                itemization_rule)
             VALUES ('{PLAN}', 0, '{TENANT_TWO}', 'Subscription: {{plan}}', '4000', 'per_plan')"
        ),
        "belongs to another tenant and may not hold this row",
    )
    .await;
    assert_eq!(
        count_sets(&conn, 0).await,
        "0",
        "the foreign-tenant row must not have landed"
    );

    // The revision's own tenant lands, so the refusal is about the tenant alone.
    must_succeed(&conn, &insert_descriptors(0, "4000")).await;

    // The UPDATE direction: the arm is on the `NEW` path and a draft's set is
    // mutable, while the frozen-column whitelist says nothing about `tenant_id`.
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE pricing_plan_descriptor_set SET tenant_id = '{TENANT_TWO}' \
             WHERE plan_id = '{PLAN}' AND plan_revision = 0"
        ),
        "belongs to another tenant and may not hold this row",
    )
    .await;
}
