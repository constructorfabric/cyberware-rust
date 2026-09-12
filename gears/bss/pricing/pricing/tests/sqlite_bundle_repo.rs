//! `BundleRepo` — creating a bundle on a plan, and replacing an open draft
//! revision's whole composition under that revision's entity tag.
//!
//! Where `tests/sqlite_bundle_store.rs` drives raw SQL past every repository to
//! prove the **schema**, this suite drives the repository, because what it proves
//! is the repository's: which refusal a caller is told, what the store holds
//! afterwards, and that the compare-and-swap is in SQL rather than in a read.
//!
//! **The composition carries no entity tag of its own.** It rides the plan
//! revision's, exactly as `PlanShapeRepo`'s child sets do and for that module's
//! reason: if editing the component set left the revision's tag alone, two
//! authors editing one draft would both satisfy `If-Match` and silently
//! interleave — the lost update `fr-concurrent-edit` exists to refuse.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::bundle::{
    Absorber, InvoiceItemization, Party, PartyShare, PriceBasis, RevShareGroup,
};
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::audit_log;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{
    BundleComponentDraft, BundleRepo, CompositionDraft, NewBundle, NewPlanDraft, PlanRepo,
};
use time::OffsetDateTime;

use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_10);

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 6, hour, 0, 0)
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: Uuid::from_u128(0xac_11),
        recorded_at: at(10),
        correlation_id: TEST_CORRELATION,
    }
}

async fn harness() -> (PlanRepo, BundleRepo, DBProvider<DbError>) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    (
        PlanRepo::new(provider.clone()),
        BundleRepo::new(provider.clone()),
        provider,
    )
}

fn new_draft(plan_id: PlanId, tenant_id: Uuid) -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id,
        tenant_id,
        created_by: Uuid::from_u128(0xac_11),
        created_at_utc: at(9),
        sku_id: Some(Uuid::from_u128(0x5_c1)),
        plan_tier: Some("gold".to_owned()),
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

const VENDOR: Uuid = Uuid::from_u128(0x_5e_11);

fn component(id: u128) -> BundleComponentDraft {
    BundleComponentDraft {
        component_plan_id: Uuid::from_u128(id),
        included_sku_id: Uuid::from_u128(id + 0x1000),
        min_qty: None,
        max_qty: None,
    }
}

fn group(shares: &[(&str, i32)], cut: i32) -> RevShareGroup {
    RevShareGroup {
        vendor_sku_id: VENDOR,
        platform_cut_bp: cut,
        residual_absorber: Absorber::Platform,
        parties: shares
            .iter()
            .map(|(n, bp)| PartyShare {
                party: Party::new(n).expect("party"),
                share_bp: *bp,
            })
            .collect(),
    }
}

/// A plan with an open draft revision 0, and a bundle on it.
async fn seeded(
    plans: &PlanRepo,
    bundles: &BundleRepo,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
) -> Uuid {
    plans
        .create_draft(scope, new_draft(plan_id, tenant))
        .await
        .expect("create the plan draft");
    bundles
        .create(
            scope,
            NewBundle {
                bundle_id: Uuid::from_u128(0xb0_1d),
                tenant_id: tenant,
                plan_id,
                price_basis: PriceBasis::SumOfParts,
                invoice_itemization: InvoiceItemization::Aggregate,
            },
            stamp(),
        )
        .await
        .expect("create the bundle")
}

/// Every audit record filed against one bundle, oldest first.
///
/// Read across tenants deliberately: a record the writer filed under the wrong
/// tenant would be invisible to a scoped read and the assertion would then be
/// about the scope rather than about the record.
async fn audit_records_for(
    provider: &DBProvider<DbError>,
    subject_ref: &str,
) -> Vec<audit_log::Model> {
    let conn = provider.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(audit_log::Column::SubjectRef.eq(subject_ref)))
        .order_by(audit_log::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read audit rows")
}

// ---------------------------------------------------------------------------
// Creating the bundle row.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_bundle_is_created_on_its_plan_and_read_back() {
    let (plans, bundles, _) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    let bundle_id = seeded(&plans, &bundles, &scope, tenant, plan_id).await;

    let found = bundles
        .find_by_plan(&scope, tenant, plan_id)
        .await
        .expect("read")
        .expect("the bundle must be there");
    assert_eq!(found.bundle_id, bundle_id);
    assert_eq!(found.price_basis, PriceBasis::SumOfParts);
    assert_eq!(found.invoice_itemization, InvoiceItemization::Aggregate);
}

/// One bundle per plan — `uq_pricing_bundle_plan`, surfaced as a typed refusal
/// rather than as a driver error.
#[tokio::test]
async fn a_second_bundle_on_one_plan_is_refused() {
    let (plans, bundles, _) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    seeded(&plans, &bundles, &scope, tenant, plan_id).await;

    let err = bundles
        .create(
            &scope,
            NewBundle {
                bundle_id: Uuid::from_u128(0xb0_2d),
                tenant_id: tenant,
                plan_id,
                price_basis: PriceBasis::OwnPrice,
                invoice_itemization: InvoiceItemization::Itemize,
            },
            stamp(),
        )
        .await
        .expect_err("one bundle per plan");

    assert!(
        matches!(err, RepoError::DuplicateBundleOnPlan { .. }),
        "unexpected: {err:?}"
    );
}

/// **One conversion, one failure answer, on every door that asks it.**
///
/// `stored_revision` returned an `Option` and left each of its four callers to
/// decide what an unstorable revision meant. They decided three different things:
/// the copy and the replace refused, the drop answered `Ok(())` for a revision it
/// never looked at, and this read answered an empty composition. A caller cannot
/// tell "cleared" from "never examined", or "no components" from "unreadable", and
/// `PlanRepo::abandon_draft` would flip the revision believing the composition was
/// gone.
///
/// The revision cannot arrive out of range through the store — the column is `i64`
/// under `CHECK (revision >= 0)` and `plan_repo::to_domain` refuses anything else
/// on the way in — so this is the seam's own contract rather than a live hole. It
/// is asserted here because the four readings were the defect, and a reading is
/// only pinned where it is executed.
#[tokio::test]
async fn an_unstorable_revision_is_one_refusal_on_every_door() {
    let (plans, bundles, _provider) = harness().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::now_v7());
    seeded(&plans, &bundles, &scope, tenant, plan_id).await;

    let unstorable = u64::try_from(i64::MAX).expect("i64::MAX is a u64") + 1;

    let read = bundles
        .load_composition(&scope, tenant, plan_id, unstorable)
        .await
        .expect_err("an unreadable revision is not an empty composition");
    assert!(
        matches!(read, RepoError::ValueOutOfRange { .. }),
        "the read must refuse rather than answer a default, and as a caller mistake \
         rather than as a table written around: {read:?}"
    );
}

/// A foreign tenant's bundle is invisible, which is `SecureORM` doing its job
/// through this repository rather than around it.
#[tokio::test]
async fn a_foreign_tenants_bundle_is_invisible() {
    let (plans, bundles, _) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    seeded(&plans, &bundles, &scope, tenant, plan_id).await;

    let stranger = AccessScope::for_tenant(Uuid::from_u128(0x7e_22));
    let found = bundles
        .find_by_plan(&stranger, Uuid::from_u128(0x7e_22), plan_id)
        .await
        .expect("read");
    assert!(found.is_none(), "a foreign tenant must see nothing");
}

/// A create naming a plan the caller cannot read is a `NotFound`, and it takes
/// nothing away from the tenant that owns the plan.
///
/// Both halves of A1-2 in one case, because a fix to either alone leaves the
/// other's damage standing. `create_on` used to read only for *a bundle*
/// (`bundle_of_plan`), never for the plan, and that read is tenant-scoped — so a
/// caller naming a foreign plan was told `DuplicateBundleOnPlan` when the owner
/// had a bundle and `201` when it did not, which is an existence oracle over
/// another tenant's plans; and in the `201` case the row landed in
/// `uq_pricing_bundle_plan`, which was globally unique on `plan_id` alone, so the
/// owner was then refused `BUNDLE_EXISTS_ON_PLAN` forever against a row it cannot
/// read, in a tenant it cannot see, with no `DELETE` anywhere in the API.
///
/// The second half is the assertion that matters and it is not decorative: it is
/// the one an index still keyed `(plan_id)` fails even with the plan read in
/// place, on any database that already holds a squatted row.
#[tokio::test]
async fn a_bundle_on_a_foreign_tenants_plan_is_refused_and_leaves_the_slot_free() {
    let (plans, bundles, _) = harness().await;
    let owner = Uuid::from_u128(0x7e_11);
    let owner_scope = AccessScope::for_tenant(owner);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    plans
        .create_draft(&owner_scope, new_draft(plan_id, owner))
        .await
        .expect("the owner's plan draft");

    let stranger = Uuid::from_u128(0x7e_22);
    let stranger_scope = AccessScope::for_tenant(stranger);
    let err = bundles
        .create(
            &stranger_scope,
            NewBundle {
                bundle_id: Uuid::from_u128(0xb0_5d),
                tenant_id: stranger,
                plan_id,
                price_basis: PriceBasis::SumOfParts,
                invoice_itemization: InvoiceItemization::Aggregate,
            },
            stamp(),
        )
        .await
        .expect_err("a plan the caller cannot read is not a plan it can bundle");
    assert!(
        matches!(&err, RepoError::NotFound { subject, .. } if subject == "plan"),
        "a foreign plan must read exactly like an absent one, not like a taken slot: {err:?}"
    );

    // The owner's own create still works — the half `uq_pricing_bundle_plan`
    // carries, and the half that made the damage irreversible.
    bundles
        .create(
            &owner_scope,
            NewBundle {
                bundle_id: Uuid::from_u128(0xb0_1d),
                tenant_id: owner,
                plan_id,
                price_basis: PriceBasis::SumOfParts,
                invoice_itemization: InvoiceItemization::Aggregate,
            },
            stamp(),
        )
        .await
        .expect("the plan's own tenant may bundle it");
}

/// An absent plan and a foreign one are the same answer.
///
/// The negative control for the case above: without it, a `NotFound` that only
/// ever fires for a plan nobody has would look identical in the log and close
/// nothing. [`RepoError::NotFound`]'s own doc is what this asserts — out of scope
/// folds into not-found so that a foreign row's existence is not confirmed.
#[tokio::test]
async fn a_bundle_on_a_plan_that_does_not_exist_is_the_same_refusal() {
    let (_, bundles, _) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);

    let err = bundles
        .create(
            &scope,
            NewBundle {
                bundle_id: Uuid::from_u128(0xb0_6d),
                tenant_id: tenant,
                plan_id: PlanId::new(Uuid::from_u128(0x9_9a9)),
                price_basis: PriceBasis::SumOfParts,
                invoice_itemization: InvoiceItemization::Aggregate,
            },
            stamp(),
        )
        .await
        .expect_err("a bundle needs a plan");
    assert!(
        matches!(&err, RepoError::NotFound { subject, .. } if subject == "plan"),
        "unexpected: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Replacing the composition under the revision's tag.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_composition_is_written_whole_and_read_back() {
    let (plans, bundles, _) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    seeded(&plans, &bundles, &scope, tenant, plan_id).await;

    let revision = bundles
        .replace_composition(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            CompositionDraft {
                components: vec![component(1), component(2)],
                rev_share_groups: vec![group(&[("a", 4500), ("b", 4500)], 1000)],
            },
            stamp(),
        )
        .await
        .expect("write the composition");

    // The revision's tag advanced: the composition rides it (see the module doc).
    assert_eq!(revision.row_version, RowVersion::new(1));

    let stored = bundles
        .load_composition(&scope, tenant, plan_id, 0)
        .await
        .expect("read the composition");
    assert_eq!(stored.components.len(), 2);
    assert_eq!(stored.rev_share_groups.len(), 1);
    assert_eq!(stored.rev_share_groups[0].parties.len(), 2);
}

/// Wholesale replacement, never a merge: a second write with one component
/// leaves **one**, not three.
///
/// The reason is `PlanShapeRepo`'s for phases, applied here: the composition is
/// judged as a set — "every referenced component", the per-market coverage walk,
/// "sum to 100% per vendor SKU" — and a partial update has no set the rules
/// could evaluate.
#[tokio::test]
async fn a_second_write_replaces_the_composition_rather_than_merging_it() {
    let (plans, bundles, _) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    seeded(&plans, &bundles, &scope, tenant, plan_id).await;

    bundles
        .replace_composition(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            CompositionDraft {
                components: vec![component(1), component(2)],
                rev_share_groups: vec![group(&[("a", 9000)], 1000)],
            },
            stamp(),
        )
        .await
        .expect("first write");
    bundles
        .replace_composition(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(1),
            CompositionDraft {
                components: vec![component(3)],
                rev_share_groups: Vec::new(),
            },
            stamp(),
        )
        .await
        .expect("second write");

    let stored = bundles
        .load_composition(&scope, tenant, plan_id, 0)
        .await
        .expect("read");
    assert_eq!(stored.components.len(), 1, "replaced, not merged");
    assert_eq!(stored.components[0].component_plan_id, Uuid::from_u128(3));
    assert!(stored.rev_share_groups.is_empty(), "the groups went too");
}

/// The compare-and-swap is the guard: a stale tag is refused and nothing is
/// written.
#[tokio::test]
async fn a_stale_row_version_is_refused_and_writes_nothing() {
    let (plans, bundles, _) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    seeded(&plans, &bundles, &scope, tenant, plan_id).await;
    bundles
        .replace_composition(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            CompositionDraft {
                components: vec![component(1)],
                rev_share_groups: Vec::new(),
            },
            stamp(),
        )
        .await
        .expect("first write");

    let err = bundles
        .replace_composition(
            &scope,
            tenant,
            plan_id,
            0,
            // The version the first write consumed.
            RowVersion::new(0),
            CompositionDraft {
                components: vec![component(9)],
                rev_share_groups: Vec::new(),
            },
            stamp(),
        )
        .await
        .expect_err("a stale tag must be refused");
    assert!(
        matches!(err, RepoError::StaleRowVersion { .. }),
        "unexpected: {err:?}"
    );

    let stored = bundles
        .load_composition(&scope, tenant, plan_id, 0)
        .await
        .expect("read");
    assert_eq!(
        stored.components[0].component_plan_id,
        Uuid::from_u128(1),
        "the refused write must have landed nothing"
    );
}

/// A published revision's composition is frozen (D-92). The repository refuses
/// before the trigger does, so the caller is told which conjunct failed.
#[tokio::test]
async fn a_published_revisions_composition_cannot_be_replaced() {
    let (plans, bundles, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    seeded(&plans, &bundles, &scope, tenant, plan_id).await;
    flip_published(&provider, &scope, plan_id).await;

    let err = bundles
        .replace_composition(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            CompositionDraft {
                components: vec![component(1)],
                rev_share_groups: Vec::new(),
            },
            stamp(),
        )
        .await
        .expect_err("a published revision is frozen");

    // **One conjunct**, which is what "the caller is told which conjunct failed"
    // claims. Accepting either variant is satisfied by the compare-and-swap
    // answering first, and then the case says nothing about the freeze at all —
    // `a_stale_row_version_is_refused_and_writes_nothing` is what covers the
    // other conjunct, on a draft where the freeze cannot answer.
    assert!(
        matches!(err, RepoError::NotDraft { .. }),
        "unexpected: {err:?}"
    );
}

/// Flip revision 0 to `published` past the repository — the state a typed path
/// reaches only through the publish commit.
async fn flip_published(provider: &DBProvider<DbError>, scope: &AccessScope, plan_id: PlanId) {
    use bss_pricing::infra::storage::entity::plan;
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;

    let conn = provider.conn().expect("conn");
    plan::Entity::update_many()
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
        .expect("flip");
}

// ---------------------------------------------------------------------------
// The collection read: `GET /bss-pricing/v1/bundles`
// ---------------------------------------------------------------------------

/// The page is a keyset walk on `bundle_id` and visits every bundle once.
///
/// Three bundles on three plans — `uq_pricing_bundle_plan` allows no other
/// arrangement — walked two at a time, so a `list_page` that ignored `after`
/// would loop on the first page rather than finish.
#[tokio::test]
async fn the_bundle_page_walks_by_bundle_id_and_never_repeats_one() {
    let (plans, bundles, _) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);

    let mut expected = Vec::new();
    for n in 0..3_u128 {
        let plan_id = PlanId::new(Uuid::from_u128(0x9_2a0 + n));
        plans
            .create_draft(&scope, new_draft(plan_id, tenant))
            .await
            .expect("create the plan draft");
        expected.push(
            bundles
                .create(
                    &scope,
                    NewBundle {
                        bundle_id: Uuid::from_u128(0xb1_00 + n),
                        tenant_id: tenant,
                        plan_id,
                        price_basis: PriceBasis::SumOfParts,
                        invoice_itemization: InvoiceItemization::Aggregate,
                    },
                    stamp(),
                )
                .await
                .expect("create the bundle"),
        );
    }

    let mut walked: Vec<Uuid> = Vec::new();
    let mut after = None;
    // **Bounded, not `loop`.** The defect this case is written against is a
    // `list_page` that ignores `after`, and such a page is never empty — so an
    // unbounded walk spins forever and grows `walked` without limit instead of
    // failing. Measured 2026-08-20: pinning `after` to `None` here left this test
    // running past 600 seconds until the runner killed it, which is a CI timeout
    // with no assertion message rather than a regression report. Three rows at two
    // per page is two pages and then the empty one; eight is slack.
    let mut drained = false;
    for _ in 0..8 {
        let page = bundles
            .list(&scope, tenant, None, after, 2)
            .await
            .expect("list");
        assert!(page.len() <= 2, "the page is bounded by the limit it asked");
        let Some(last) = page.last() else {
            drained = true;
            break;
        };
        after = Some(last.bundle_id);
        walked.extend(page.iter().map(|record| record.bundle_id));
    }
    assert!(
        drained,
        "the page never advanced: eight pages of two and the walk still had rows, which is what \
         a keyset read that ignored `after` looks like ({walked:?})"
    );

    assert_eq!(walked, expected, "every bundle once, in bundle_id order");
}

/// `plan_id` narrows the page to the bundle riding one plan.
///
/// **`lifecycle_state` is not the filter and cannot be**: `pricing_bundle` holds
/// `bundle_id`, `tenant_id`, `plan_id`, `price_basis` and `invoice_itemization`
/// and no lifecycle column at all — the composition is revision-scoped and the
/// bundle row is the identity that rides it.
///
/// The middle plan is named rather than the first, so a query that dropped the
/// condition and returned the whole page would fail rather than coincide.
#[tokio::test]
async fn the_bundle_page_narrows_to_one_plan() {
    let (plans, bundles, _) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);

    for n in 0..3_u128 {
        let plan_id = PlanId::new(Uuid::from_u128(0x9_3a0 + n));
        plans
            .create_draft(&scope, new_draft(plan_id, tenant))
            .await
            .expect("create the plan draft");
        bundles
            .create(
                &scope,
                NewBundle {
                    bundle_id: Uuid::from_u128(0xb2_00 + n),
                    tenant_id: tenant,
                    plan_id,
                    price_basis: PriceBasis::OwnPrice,
                    invoice_itemization: InvoiceItemization::Itemize,
                },
                stamp(),
            )
            .await
            .expect("create the bundle");
    }
    let wanted = PlanId::new(Uuid::from_u128(0x9_3a1));

    let page = bundles
        .list(&scope, tenant, Some(wanted), None, 100)
        .await
        .expect("list");

    assert_eq!(
        page.iter()
            .map(|record| record.bundle_id)
            .collect::<Vec<_>>(),
        vec![Uuid::from_u128(0xb2_01)],
        "only the named plan's bundle"
    );
    assert_eq!(
        page[0].price_basis,
        PriceBasis::OwnPrice,
        "and the stored basis is parsed, not defaulted"
    );
    assert_eq!(page[0].invoice_itemization, InvoiceItemization::Itemize);
}

/// A bundle's coming into existence leaves a record, like every edit to what it
/// contains already did.
///
/// The create took an `AuditStamp` and discarded it, so an auditor could see
/// every change to a bundle's composition and nothing at all about the bundle —
/// who created it, when, or under which request. The three the stamp carries are
/// asserted individually rather than through a count: a record written with a
/// null actor or a null correlation is a row that satisfies "one record was
/// written" and answers none of the questions the trail is read for.
#[tokio::test]
async fn creating_a_bundle_writes_exactly_one_audit_record() {
    let (plans, bundles, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    let bundle_id = seeded(&plans, &bundles, &scope, tenant, plan_id).await;

    let records = audit_records_for(&provider, &bundle_id.to_string()).await;
    assert_eq!(records.len(), 1, "one create is one act");
    assert_eq!(records[0].action, "create");
    assert_eq!(records[0].tenant_id, tenant);
    assert_eq!(records[0].actor_principal_id, stamp().actor_principal_id);
    assert_eq!(records[0].recorded_at, stamp().recorded_at);
    assert_eq!(records[0].correlation_id, Some(TEST_CORRELATION));
}

/// A refused create writes none — the bundle does not exist, so neither does the
/// record.
///
/// The positive control's twin. Without it the case above passes against a writer
/// that appends unconditionally, which would file a record for a bundle no caller
/// can read.
#[tokio::test]
async fn a_refused_create_writes_no_audit_record() {
    let (plans, bundles, provider) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    seeded(&plans, &bundles, &scope, tenant, plan_id).await;

    let refused = Uuid::from_u128(0xb0_2d);
    bundles
        .create(
            &scope,
            NewBundle {
                bundle_id: refused,
                tenant_id: tenant,
                plan_id,
                price_basis: PriceBasis::OwnPrice,
                invoice_itemization: InvoiceItemization::Itemize,
            },
            stamp(),
        )
        .await
        .expect_err("one bundle per plan");

    assert!(
        audit_records_for(&provider, &refused.to_string())
            .await
            .is_empty(),
        "the refused create wrote nothing"
    );
}
