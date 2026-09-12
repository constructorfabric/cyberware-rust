//! `BundleService::publish_composition` — D-07's normalisation, as a **write**
//! (`design/08-bundles.md` §2, `inst-rs-residual`, `inst-ba-return`).
//!
//! Where `tests/sqlite_bundle_repo.rs` drives the composition's authoring and
//! `src/domain/bundle_tests.rs` drives the reconciler's arithmetic, this suite
//! drives the one thing neither can: the statement that puts the reconciled
//! shares in the store, and what happens when it addresses nothing.
//!
//! **Nothing exercised that statement before this file.** `rest_bundles`' publish
//! case runs an `own_price` bundle, which carries no rev-share group at all, so
//! `write_effective_share` had no caller under any green tree — the column
//! downstream vendors are paid on was written by code no test ever ran.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::bundle::{
    Absorber, InvoiceItemization, Party, PartyShare, PriceBasis, RevShareGroup,
};
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::bundle::BundleService;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::{audit_log, bundle_revshare, outbox};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{
    BundleComponentDraft, BundleRepo, CompositionDraft, NewBundle, NewPlanDraft, PlanRepo,
};
use time::OffsetDateTime;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureDeleteExt, SecureEntityExt, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_b1);
const ACTOR: Uuid = Uuid::from_u128(0xac_b1);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_b1);
/// A **second** operator call's correlation. D-178 mints one per request at the
/// edge, so two calls never share one — see the Z8-3 case.
const CORRELATION_2: Uuid = Uuid::from_u128(0xc0_b2);
const VENDOR: Uuid = Uuid::from_u128(0x5e_b1);
const BUNDLE: Uuid = Uuid::from_u128(0xb0_b1);
const PLAN: Uuid = Uuid::from_u128(0x91_b1);

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 13, hour, 0, 0)
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

fn plan() -> PlanId {
    PlanId::new(PLAN)
}

fn stamp() -> AuditStamp {
    stamp_at(CORRELATION, at(10))
}

/// One operator call's stamp. The correlation and the instant vary per case —
/// D-178 mints a correlation per request, and two publishes of one revision are
/// two acts — while the actor does not: nothing here is about who acted.
fn stamp_at(correlation_id: Uuid, recorded_at: OffsetDateTime) -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at,
        correlation_id,
    }
}

struct Harness {
    provider: DBProvider<DbError>,
    plans: PlanRepo,
    bundles: BundleRepo,
    service: BundleService,
}

async fn harness() -> Harness {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    // **The no-op metrics port, deliberately.** `BundleService` reports from
    // `validate_publish` alone, and every call this suite makes is
    // `publish_composition`, which touches no instrument — so a real port here
    // would be wired, never written to, and never read: an `#[allow(dead_code)]`
    // field advertising coverage the suite does not have. What does observe the
    // one instrument is `rest_bundles`, over the REST door that calls
    // `validate_publish`, with negative controls.
    Harness {
        provider: provider.clone(),
        plans: PlanRepo::new(provider.clone()),
        bundles: BundleRepo::new(provider.clone()),
        service: BundleService::new(provider),
    }
}

/// A plan with an open draft revision 0 and a bundle on it.
///
/// The revision stays **draft** on purpose: `pricing_bundle_revshare`'s
/// append-only trigger refuses an `UPDATE` under a non-draft plan revision, so a
/// published revision would make the statement under test fail for a reason that
/// is not this suite's.
async fn seeded(h: &Harness) {
    h.plans
        .create_draft(
            &scope(),
            NewPlanDraft {
                plan_name: None,
                plan_id: plan(),
                tenant_id: TENANT,
                created_by: ACTOR,
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
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("create the plan draft");
    h.bundles
        .create(
            &scope(),
            NewBundle {
                bundle_id: BUNDLE,
                tenant_id: TENANT,
                plan_id: plan(),
                price_basis: PriceBasis::SumOfParts,
                invoice_itemization: InvoiceItemization::Aggregate,
            },
            stamp(),
        )
        .await
        .expect("create the bundle");
}

fn component(id: u128) -> BundleComponentDraft {
    BundleComponentDraft {
        component_plan_id: Uuid::from_u128(id),
        included_sku_id: Uuid::from_u128(id + 0x1000),
        min_qty: None,
        max_qty: None,
    }
}

/// Every stored party row of revision 0, as `(party, share_bp, effective)`,
/// **in `party` order**.
///
/// Ordered for [`announcement_rows`]'s reason: three cases compare this against
/// an exact `Vec`, and an unordered `SELECT` promises no order at all. Measured
/// 2026-08-20 — reversing the order here reddens two of them, so the order was
/// load-bearing while nothing asked for it.
async fn stored_shares(h: &Harness) -> Vec<(String, i32, Option<i32>)> {
    let conn = h.provider.conn().expect("conn");
    bundle_revshare::Entity::find()
        .secure()
        .scope_with(&scope())
        .filter(
            Condition::all()
                .add(bundle_revshare::Column::TenantId.eq(TENANT))
                .add(bundle_revshare::Column::BundleId.eq(BUNDLE))
                .add(bundle_revshare::Column::PlanRevision.eq(0_i64)),
        )
        .order_by(bundle_revshare::Column::Party, Order::Asc)
        .all(&conn)
        .await
        .expect("read the party rows")
        .into_iter()
        .map(|row| (row.party, row.share_bp, row.effective_share_bp))
        .collect()
}

/// Every `BundleUpdated` the outbox holds for this plan.
async fn announcements(h: &Harness) -> Vec<serde_json::Value> {
    announcement_rows(h)
        .await
        .into_iter()
        .map(|r| r.1)
        .collect()
}

/// Every `BundleUpdated` as `(dedup_key, payload)`, in `seq` order.
async fn announcement_rows(h: &Harness) -> Vec<(String, serde_json::Value)> {
    let conn = h.provider.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&scope())
        .filter(
            Condition::all()
                .add(outbox::Column::TenantId.eq(TENANT))
                .add(outbox::Column::EventName.eq("BundleUpdated")),
        )
        .order_by(outbox::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the outbox")
        .into_iter()
        .map(|row| (row.dedup_key, row.payload))
        .collect()
}

// ---------------------------------------------------------------------------
// The positive control: the statement does write, and writes the reconciled
// value rather than the typed one.
// ---------------------------------------------------------------------------

/// D-07's residual lands on the nominated party and the **effective** column is
/// what moves; the typed one is retained for audit.
///
/// The absorbed residual is 1 bp because that is the whole admissible range:
/// `reconcile` refuses anything further than `RESIDUAL_TOLERANCE_BP` from the
/// whole, so a group publish admits at all is a group whose authored sum is
/// 9999, 10000 or 10001.
#[tokio::test]
async fn the_reconciled_shares_are_written_to_the_effective_column_on_publish() {
    let h = harness().await;
    seeded(&h).await;

    let absorber = Party::new("acme").expect("party");
    h.bundles
        .replace_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(0),
            CompositionDraft {
                components: vec![component(1)],
                rev_share_groups: vec![RevShareGroup {
                    vendor_sku_id: VENDOR,
                    platform_cut_bp: 1_000,
                    residual_absorber: Absorber::Party(absorber.clone()),
                    parties: vec![
                        PartyShare {
                            party: absorber,
                            share_bp: 4_499,
                        },
                        PartyShare {
                            party: Party::new("globex").expect("party"),
                            share_bp: 4_500,
                        },
                    ],
                }],
            },
            stamp(),
        )
        .await
        .expect("author the composition");

    assert_eq!(
        stored_shares(&h).await,
        vec![
            ("acme".to_owned(), 4_499, None),
            ("globex".to_owned(), 4_500, None),
        ],
        "unpublished, the effective column is absent, which is what `None` means here"
    );

    h.service
        .publish_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(1),
            stamp_at(CORRELATION, at(11)),
        )
        .await
        .expect("publish the composition");

    assert_eq!(
        stored_shares(&h).await,
        vec![
            ("acme".to_owned(), 4_499, Some(4_500)),
            ("globex".to_owned(), 4_500, Some(4_500)),
        ],
        "the absorber took the 1 bp residual and the typed value stands beside it"
    );
    assert_eq!(
        announcements(&h).await.len(),
        1,
        "and the composition change was announced once"
    );
}

/// Every audit record filed against this bundle, oldest first.
///
/// Read across tenants deliberately: a record filed under the wrong tenant would
/// be invisible to a scoped read, and the assertion would then be about the scope
/// rather than about the record.
async fn audit_records(h: &Harness) -> Vec<audit_log::Model> {
    let conn = h.provider.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(audit_log::Column::SubjectRef.eq(BUNDLE.to_string())))
        .order_by(audit_log::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the audit rows")
}

/// The one act that decides how money is split between vendor parties leaves a
/// record, inside the transaction that splits it.
///
/// It appended nothing, while all seven sibling publish units did — so the stored
/// `effective_share_bp` a vendor is paid on had no trail naming who published it
/// or under which request. The record must be **inside** the write's transaction:
/// appended afterwards, a crash between the two leaves money split with no record
/// of who split it, which is the state D-135 puts the append inside the mutation
/// to make unreachable.
#[tokio::test]
async fn publishing_a_revshare_composition_writes_exactly_one_audit_record() {
    let h = harness().await;
    seeded(&h).await;

    let absorber = Party::new("acme").expect("party");
    h.bundles
        .replace_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(0),
            CompositionDraft {
                components: vec![component(1)],
                rev_share_groups: vec![RevShareGroup {
                    vendor_sku_id: VENDOR,
                    platform_cut_bp: 1_000,
                    residual_absorber: Absorber::Party(absorber.clone()),
                    parties: vec![
                        PartyShare {
                            party: absorber,
                            share_bp: 4_499,
                        },
                        PartyShare {
                            party: Party::new("globex").expect("party"),
                            share_bp: 4_500,
                        },
                    ],
                }],
            },
            stamp(),
        )
        .await
        .expect("author the composition");

    // The create's record, and nothing else on this ref yet: the composition edit
    // is filed against the plan revision, not against the bundle.
    let before = audit_records(&h).await;
    assert_eq!(
        before.iter().map(|r| r.action.as_str()).collect::<Vec<_>>(),
        vec!["create"],
        "the bundle's own ref carries its creation and no publish"
    );

    h.service
        .publish_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(1),
            stamp_at(CORRELATION_2, at(11)),
        )
        .await
        .expect("publish the composition");

    let records = audit_records(&h).await;
    assert_eq!(
        records
            .iter()
            .map(|r| r.action.as_str())
            .collect::<Vec<_>>(),
        vec!["create", "publish"],
        "one publish is one act"
    );
    let published = records.last().expect("the publish record");
    assert_eq!(published.tenant_id, TENANT);
    assert_eq!(published.actor_principal_id, ACTOR);
    assert_eq!(published.recorded_at, at(11));
    assert_eq!(published.correlation_id, Some(CORRELATION_2));
    // The authored split and the reconciled one, both, because what an auditor
    // has to be able to answer is *what changed* — the residual moved 4_499 to
    // 4_500 on the absorber, and a record carrying only one side cannot say so.
    assert_eq!(
        published.before_state,
        Some(serde_json::json!({
            "revShareGroups": [{
                "vendorSkuId": VENDOR.to_string(),
                "platformCutBp": 1_000,
                "parties": [
                    { "party": "acme", "shareBp": 4_499 },
                    { "party": "globex", "shareBp": 4_500 },
                ],
            }],
        }))
    );
    assert_eq!(
        published.after_state,
        Some(serde_json::json!({
            "revShareGroups": [{
                "vendorSkuId": VENDOR.to_string(),
                // The authored shares are 1 bp short of the whole, so the residual
                // the absorber took is `+1` and the platform cut is untouched.
                "adjustmentBp": 1,
                "effectivePlatformCutBp": 1_000,
                "parties": [
                    { "party": "acme", "effectiveShareBp": 4_500 },
                    { "party": "globex", "effectiveShareBp": 4_500 },
                ],
            }],
        }))
    );
}

/// A publish refused by the version compare writes no record — nothing happened.
///
/// The refusal arm's positive control. Without it the case above passes against a
/// writer that appends before the compare, which would file a record for a split
/// that never landed.
#[tokio::test]
async fn a_publish_refused_by_the_version_compare_writes_no_audit_record() {
    let h = harness().await;
    seeded(&h).await;

    h.service
        .publish_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(7),
            stamp_at(CORRELATION_2, at(11)),
        )
        .await
        .expect_err("the pinned version never stood");

    assert_eq!(
        audit_records(&h)
            .await
            .iter()
            .map(|r| r.action.as_str())
            .collect::<Vec<_>>(),
        vec!["create"],
        "the refused publish wrote nothing"
    );
}

// ---------------------------------------------------------------------------
// Z9-6 — the zero-row outcome, which was indistinguishable from success.
// ---------------------------------------------------------------------------

/// **The RED this case is about.** `write_effective_share` was a blind
/// `update_many` whose `UpdateResult` was dropped on the floor: a filter matching
/// no row left the stored `effective_share_bp` at whatever it was, the act
/// answered `Ok(())`, and a `BundleUpdated` event announced a composition whose
/// reconciled shares were never stored. Downstream vendors are paid on that
/// column.
///
/// The way in, here, is the party name. `chk_pricing_bundle_revshare_party` refuses
/// a value that trims to nothing and one that trims to the `platform` sentinel, and
/// **admits a padded party** — it does not require the stored spelling to equal the
/// trimmed one — while `Party::new` trims on the way out
/// (`bundle_repo::read_composition` builds every group member through it). So a
/// row written around this repository (a data fix, a restore, a migration from
/// another shape) reads back under a name the write cannot address, and the whole
/// group's normalisation silently does not happen. Any other route to a
/// non-matching `(bundle, revision, vendor_sku, party)` filter has the same
/// ending; this one is deterministic.
///
/// Armed at the **zero-row** case: the reconciliation is well-formed and the
/// event is the thing that must not be enqueued. A case that only proved the
/// normal write still works would prove nothing about this.
#[tokio::test]
async fn a_reconciled_share_the_write_cannot_address_is_refused_rather_than_announced() {
    let h = harness().await;
    seeded(&h).await;

    // The group is authored through the repository so the FK the party row needs
    // exists and the revision is the one the publish will load.
    h.bundles
        .replace_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(0),
            CompositionDraft {
                components: vec![component(1)],
                rev_share_groups: vec![RevShareGroup {
                    vendor_sku_id: VENDOR,
                    platform_cut_bp: 1_000,
                    residual_absorber: Absorber::Platform,
                    parties: vec![PartyShare {
                        party: Party::new("acme").expect("party"),
                        share_bp: 9_000,
                    }],
                }],
            },
            stamp(),
        )
        .await
        .expect("author the composition");

    // Now replace that party row with one the store admits and the write cannot
    // name: the same party, spelled with the leading space no CHECK forbids.
    let conn = h.provider.conn().expect("conn");
    bundle_revshare::Entity::delete_many()
        .secure()
        .scope_with(&scope())
        .filter(
            Condition::all()
                .add(bundle_revshare::Column::TenantId.eq(TENANT))
                .add(bundle_revshare::Column::BundleId.eq(BUNDLE))
                .add(bundle_revshare::Column::Party.eq("acme")),
        )
        .exec(&conn)
        .await
        .expect("clear the well-spelled row");
    let padded = bundle_revshare::ActiveModel {
        bundle_id: Set(BUNDLE),
        plan_revision: Set(0),
        vendor_sku_id: Set(VENDOR),
        party: Set(" acme".to_owned()),
        tenant_id: Set(TENANT),
        share_bp: Set(9_000),
        effective_share_bp: Set(None),
    };
    bundle_revshare::Entity::insert(padded.clone())
        .secure()
        .scope_with_model(&scope(), &padded)
        .expect("scope the seed")
        .exec(&conn)
        .await
        .expect("the party CHECK admits a padded name, which is the premise here");

    // Sanity: the group still reconciles, so nothing below is a reconciliation
    // refusal wearing this case's name.
    let draft = h
        .bundles
        .load_composition(&scope(), TENANT, plan(), 0)
        .await
        .expect("read the composition");
    assert_eq!(draft.rev_share_groups.len(), 1);
    assert_eq!(
        draft.rev_share_groups[0].parties[0].party.get(),
        "acme",
        "the read trims, which is exactly why the write addresses nothing"
    );

    let refused = h
        .service
        .publish_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(1),
            stamp_at(CORRELATION, at(11)),
        )
        .await
        .expect_err(
            "a normalisation that stored nothing is not a publish; answering `Ok` leaves the \
             effective column stale and the event asserts a change that did not happen",
        );
    let rendered = format!("{refused:?}");
    assert!(
        matches!(
            refused,
            RepoError::NotFound { .. } | RepoError::CorruptRow(_)
        ),
        "the refusal names the row the write could not find: {rendered}"
    );
    assert!(
        rendered.contains("acme") && rendered.contains(&VENDOR.to_string()),
        "and it names the four-column key, because that is what an operator has to look \
         at: {rendered}"
    );

    assert_eq!(
        stored_shares(&h).await,
        vec![(" acme".to_owned(), 9_000, None)],
        "the effective column is still absent, which is the state the `Ok` was hiding"
    );
    assert!(
        announcements(&h).await.is_empty(),
        "and nothing announced a composition change that never reached the store"
    );
}

// ---------------------------------------------------------------------------
// Z8-3 — the second publish of one revision, which had no answer but "retry".
// ---------------------------------------------------------------------------

/// **The RED this case is about.** The announcement's dedup key was
/// `BundleUpdated:<bundle>:<revision>`, which asserts *"a revision publishes
/// exactly once"* — true of a plan revision, whose publish commit holds a
/// compare-and-swap, and **false of a bundle composition**. `publish_composition`
/// has no swap: it re-reads the composition, re-normalises and enqueues, and the
/// publish route admits a second call at one `plan_revision` (a composition edit
/// moves the revision's `row_version`, which invalidates the content pin, so the
/// legal republish is *another approved unit over the same revision*). So the
/// outbox's unique index was the only thing standing there and it answered
/// `CONCURRENT_MUTATION` — a 409 that tells the operator to retry, and every
/// retry collides identically. `outbox_repo`'s own file records this defect
/// class, diagnosed and fixed one event over: *"an adjustment of a window that was
/// scheduled through the route deduped against its own schedule … the reason no
/// window could be adjusted twice"*.
///
/// **Two correlation ids, because that is what two operator calls are.** D-178
/// establishes one correlation per request at the HTTP edge and
/// `require_correlation` refuses to mint anywhere else, so two publishes are two
/// acts by construction and the second is not a replay of the first. The tail of
/// this case drives the replay — one act's enqueue, twice — and asserts the index
/// still refuses it, so the key that stopped over-deduping did not stop
/// deduplicating.
#[tokio::test]
async fn a_second_publish_of_one_revision_makes_progress_rather_than_answering_retry_forever() {
    let h = harness().await;
    seeded(&h).await;

    h.bundles
        .replace_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(0),
            CompositionDraft {
                components: vec![component(1)],
                rev_share_groups: vec![RevShareGroup {
                    vendor_sku_id: VENDOR,
                    platform_cut_bp: 1_000,
                    residual_absorber: Absorber::Platform,
                    parties: vec![PartyShare {
                        party: Party::new("acme").expect("party"),
                        share_bp: 9_000,
                    }],
                }],
            },
            stamp(),
        )
        .await
        .expect("author the composition");

    h.service
        .publish_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(1),
            stamp_at(CORRELATION, at(11)),
        )
        .await
        .expect("the first publish of the revision");

    // The second act over the same `(bundle, plan_revision)` — the whole of the
    // finding.
    let second = h
        .service
        .publish_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(1),
            stamp_at(CORRELATION_2, at(12)),
        )
        .await;
    assert!(
        !matches!(second, Err(RepoError::ConcurrentMutation { .. })),
        "a second publish at one revision is a legal act, so it must not be answered with the \
         one status whose meaning is `retry` — retrying collides identically, forever: {second:?}"
    );
    second.expect("the second publish of one revision commits");

    let rows = announcement_rows(&h).await;
    assert_eq!(
        rows.len(),
        2,
        "each act announced its own normalisation; one row would mean a consumer never learned \
         the composition moved a second time"
    );
    assert_ne!(
        rows[0].0, rows[1].0,
        "and the two acts carry two dedup keys, which is what makes the second insertable"
    );
    assert!(
        rows.iter()
            .all(|(key, _)| key.starts_with("BundleUpdated/")),
        "the key is rendered from the enum and separated as its ten siblings are: {rows:?}"
    );

    // The positive control: the index still refuses **one** act announced twice.
    // Without this, "the second publish now fits" could equally describe a key
    // that dedups nothing at all.
    let replay = h
        .service
        .publish_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(1),
            stamp_at(CORRELATION_2, at(13)),
        )
        .await;
    assert!(
        matches!(replay, Err(RepoError::OutboxEventAlreadyEnqueued { .. })),
        "one act enqueues one announcement, so the same act's second enqueue is still refused by \
         `uq_pricing_outbox_dedup_key` — and named as the dedup index rather than folded into the \
         sequence race, because this refusal is deterministic: {replay:?}"
    );
    assert_eq!(
        announcement_rows(&h).await.len(),
        2,
        "and the refused replay added nothing"
    );
}

// ---------------------------------------------------------------------------
// C7-1 — the composition that moved after the reviewer decided over it.
// ---------------------------------------------------------------------------

/// **This is an approval bypass, not a lost update, and that is why it is here.**
///
/// `api::rest::bundles` computes `bundle_content_hash(shape, draft.row_version)`,
/// matches an approved unit on that digest, and only then calls
/// `publish_composition` — which used to re-read the composition on a **third**
/// connection, outside its own writing transaction. So an ordinary authorised
/// `PATCH …/bundles/{id}` landing between the match and the read left the act
/// reconciling and paying out a composition no second principal had seen, while
/// `BundleUpdated` announced it. The column being written is what downstream
/// vendor parties are paid on.
///
/// The version the pin was taken over is now handed down and compared inside the
/// writing transaction, so the moved composition is `StaleRowVersion` — the
/// contention refusal the act previously reported as `CorruptRow` when the
/// divergence happened to strand a party row (Z9-6) and reported not at all when
/// it did not.
///
/// **The edit here changes the split and not only the version**, so a fix that
/// compared versions but still wrote from the stale read would be caught by the
/// share assertion as well as by the refusal.
#[tokio::test]
async fn a_composition_that_moved_after_the_pin_is_refused_rather_than_paid_out() {
    let h = harness().await;
    seeded(&h).await;

    let reviewed = |acme: i32, globex: i32| CompositionDraft {
        components: vec![component(1)],
        rev_share_groups: vec![RevShareGroup {
            vendor_sku_id: VENDOR,
            platform_cut_bp: 1_000,
            residual_absorber: Absorber::Platform,
            parties: vec![
                PartyShare {
                    party: Party::new("acme").expect("party"),
                    share_bp: acme,
                },
                PartyShare {
                    party: Party::new("globex").expect("party"),
                    share_bp: globex,
                },
            ],
        }],
    };

    // What the second principal decided over: an even split, at version 0 -> 1.
    h.bundles
        .replace_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(0),
            reviewed(4_500, 4_500),
            stamp(),
        )
        .await
        .expect("author the reviewed composition");
    let pinned = RowVersion::new(1);

    // The edit nobody reviewed, landing between the approve and the publish:
    // 9 000 of 10 000 bp moves from one party to the other, 1 -> 2.
    h.bundles
        .replace_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            pinned,
            reviewed(8_999, 1),
            stamp(),
        )
        .await
        .expect("an ordinary authorised composition edit");

    let refused = h
        .service
        .publish_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            pinned,
            stamp_at(CORRELATION, at(11)),
        )
        .await
        .expect_err("the composition the reviewer decided over is not the one in the store");
    assert!(
        matches!(
            refused,
            RepoError::StaleRowVersion {
                current: 2,
                submitted: 1,
                ..
            }
        ),
        "named as contention, with both versions, so the surface can answer 409 and the operator \
         knows to re-review rather than to retry: {refused:?}"
    );

    // Nothing was paid out and nothing was announced. `None` in the third slot is
    // the effective column still absent — the writes never ran.
    assert_eq!(
        stored_shares(&h).await,
        vec![
            ("acme".to_owned(), 8_999, None),
            ("globex".to_owned(), 1, None),
        ],
        "the unreviewed split was not normalised onto the column vendors are paid on"
    );
    assert!(
        announcements(&h).await.is_empty(),
        "and no BundleUpdated announced a composition that did not publish"
    );

    // **The positive control**, and it is what keeps this case from passing on a
    // `publish_composition` that simply refuses everything: the same act at the
    // version the store now holds publishes, and normalises the *stored* split.
    h.service
        .publish_composition(
            &scope(),
            TENANT,
            plan(),
            0,
            RowVersion::new(2),
            stamp_at(CORRELATION_2, at(12)),
        )
        .await
        .expect("the composition that is actually current publishes");
    assert_eq!(
        stored_shares(&h).await,
        vec![
            ("acme".to_owned(), 8_999, Some(8_999)),
            ("globex".to_owned(), 1, Some(1)),
        ],
        "and what it normalised is what it read inside its own transaction"
    );
}
