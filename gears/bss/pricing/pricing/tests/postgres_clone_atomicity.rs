//! The clone is one act on Postgres too (D-272, D-275).
//!
//! **Why this exists as its own suite.** `tests/sqlite_clone.rs` proves the whole
//! copy set and proves the rollback, but it proves them on `SQLite`. What the
//! clone does between BEGIN and the caller's failure is **nine** write functions
//! issuing DELETEs and INSERTs across **eleven** tables — the plan, its four
//! child shape tables, the four bundle tables, the price row, and the D-135 audit
//! log every one of those writers appends to. Eight of the eleven carry
//! append-only triggers and `CHECK` constraints this gear spells **twice**, once
//! in PL/pgSQL and once as `RAISE(ABORT, …)`, and two spellings are only ever as
//! equal as the suites that hold them to it. (`pricing_bundle` itself carries
//! none — only its three children do — and `pricing_price_tier_band` is a twelfth
//! table, reached when a copied row has bands, which this seed's flat row does
//! not.)
//!
//! So the seed carries phases, an add-on rule, a descriptor set, a composite
//! meter, a bundle with two components and a rev-share group, and a published
//! price row: enough that **every** writer `clone_plan_on` can reach actually
//! runs here. It began as a plan and one price row, which reached three writers
//! of nine and skipped every guarded branch — a seed small enough to make the
//! suite agree with itself while testing almost none of what it claimed. The
//! assertions came second and were narrower than the seed for one round: the
//! add-on rule and the composition were authored and never read back, so two of
//! those writers could have been deleted with this suite still green.
//!
//! **What is under test is the rollback arm, not statement failure.** Both cases
//! run the clone to completion and then fail the *closure*; no SQL statement
//! fails, so the engines' famous difference — `SQLite` carrying on where Postgres
//! aborts the transaction — is not what is engaged. `Db::in_transaction`'s
//! `Err ⇒ rollback` is engine-independent. What is engine-specific is everything
//! the writers touched on the way there, which is the reason above.
//!
//! The failure is the **caller's**. Every failure the clone could raise from its
//! own data is unreachable: of the four scope-key axes the reset changes — plan,
//! phase, eligibility, cohort — the two that could collapse two source rows onto
//! one target key are operand-free by the time a row reaches the copy (D-266,
//! D-268), and the phase remap is injective. This is also the route's own shape,
//! where `idempotent::guarded` owns the transaction and its bookkeeping is what
//! fails after the clone.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod pg_support;

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::audit::AuditSubjectKind;
use bss_pricing::domain::bundle::{
    Absorber, InvoiceItemization, Party, PartyShare, PriceBasis, RevShareGroup,
};
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::plan_shape::{
    AddonRule, CompositeMeter, DescriptorSet, PhaseKind, PlanPhase,
};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::clone::clone_plan_on;
use bss_pricing::infra::storage::repo::{
    BundleComponentDraft, BundleRepo, CompositionDraft, NewBundle, NewPlanDraft, NewPriceDraft,
    PlanRepo, PlanShapeRepo, PriceRepo, audit_repo, bundle_repo, plan_repo, plan_shape_repo,
    price_repo,
};
use time::OffsetDateTime;

use pg_support::Pg;
use std::collections::BTreeMap;
use toolkit_db::secure::{AccessScope, TxError};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_21);
const ACTOR: Uuid = Uuid::from_u128(0xac_20);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_21);
const SOURCE_ROW: Uuid = Uuid::from_u128(0xb_1001);
const SOURCE_BUNDLE: Uuid = Uuid::from_u128(0xb_21d);
const SOURCE_COMPOSITE: Uuid = Uuid::from_u128(0xc0_f2);
const COMPONENT_PLAN: Uuid = Uuid::from_u128(0xc0_a2);
const COMPONENT_PLAN_B: Uuid = Uuid::from_u128(0xc0_b2);
const ADDON_SKU: Uuid = Uuid::from_u128(0xad_01);
const VENDOR_SKU: Uuid = Uuid::from_u128(0x5e_21);

fn source_plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x50_c2))
}
fn target_plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x7a_6a))
}
fn trial_phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0xfa_60))
}
fn terminal_phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0xfa_6e))
}
fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 8, hour, 0, 0)
}
fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}
fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: at(10),
        correlation_id: CORRELATION,
    }
}

fn source_key() -> ScopeKey {
    ScopeKey::new(
        source_plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        trial_phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with the cohort")
}

fn flat_row() -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(9_900).expect("a non-negative amount"));
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

/// A published plan revision carrying every child class the clone can copy.
///
/// Phases (a two-link chain, so the remap has something to remap), an add-on
/// rule, a descriptor set, a composite meter, a bundle with two components and a
/// rev-share group, and one published price row. Each exists so that the writer
/// that copies it actually runs: with a smaller source, `clone_plan_on`'s guarded
/// branches are skipped and the rollback under test rolls back almost nothing.
///
/// Both publishes are direct `UPDATE`s past the engine, `tests/common`'s helpers,
/// for their stated reason: this suite is about the clone, and a fixture that went
/// through the publish pipeline would make a pipeline failure look like a clone
/// failure.
async fn seed(provider: &DBProvider<DbError>) {
    let created = PlanRepo::new(provider.clone())
        .create_draft(
            &scope(),
            NewPlanDraft {
                plan_name: None,
                plan_id: source_plan(),
                tenant_id: TENANT,
                created_by: ACTOR,
                created_at_utc: at(10),
                sku_id: Some(Uuid::from_u128(0x5_c2)),
                plan_tier: Some("gold".to_owned()),
                billing_cycle: None,
                frequency: None,
                plan_tier_override: false,
                purchase_min_qty: None,
                purchase_max_qty: None,
                invoice_grouping_key: Some("group/pg-source".to_owned()),
                available_from: None,
                available_to: None,
                cloned_from: None,
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("create the source draft");

    let shapes = PlanShapeRepo::new(provider.clone());
    let after_phases = shapes
        .replace_phases(
            &scope(),
            TENANT,
            source_plan(),
            created.revision,
            created.row_version,
            vec![
                PlanPhase {
                    phase_id: trial_phase(),
                    kind: PhaseKind::Trial,
                    display_name: None,
                    ordinal: 0,
                    converts_to_phase_id: Some(terminal_phase()),
                    phase_duration_days: Some(14),
                    display_trial_days: Some(14),
                },
                PlanPhase {
                    phase_id: terminal_phase(),
                    kind: PhaseKind::Evergreen,
                    display_name: None,
                    ordinal: 1,
                    converts_to_phase_id: None,
                    phase_duration_days: None,
                    display_trial_days: None,
                },
            ],
            stamp(),
        )
        .await
        .expect("attach the chain");

    let after_rules = shapes
        .replace_addon_rules(
            &scope(),
            TENANT,
            source_plan(),
            created.revision,
            after_phases.row_version,
            vec![AddonRule {
                addon_sku_id: ADDON_SKU,
                required: true,
                min_qty: Some(1),
                max_qty: Some(3),
                step_qty: Some(1),
                price_override_ref: None,
                depends_on: Vec::new(),
                conflicts_with: Vec::new(),
            }],
            stamp(),
        )
        .await
        .expect("attach the add-on rule");

    let after_descriptors = shapes
        .set_descriptor_set(
            &scope(),
            TENANT,
            source_plan(),
            created.revision,
            after_rules.row_version,
            DescriptorSet {
                invoice_line_template: Some("{plan}".to_owned()),
                gl_code: Some("4000".to_owned()),
                itemization_rule: Some("per_charge".to_owned()),
                additional: BTreeMap::new(),
            },
            stamp(),
        )
        .await
        .expect("attach the descriptor set");

    let after_composites = shapes
        .replace_composites(
            &scope(),
            TENANT,
            source_plan(),
            created.revision,
            after_descriptors.row_version,
            vec![CompositeMeter {
                composite_id: SOURCE_COMPOSITE,
                output_unit: "vm-hour".to_owned(),
                constituent_units: vec!["vcpu-hour".to_owned(), "ram-gb-hour".to_owned()],
                formula: serde_json::json!({ "op": "weighted_sum" }),
            }],
            stamp(),
        )
        .await
        .expect("attach the composite");

    let bundles = BundleRepo::new(provider.clone());
    bundles
        .create(
            &scope(),
            NewBundle {
                bundle_id: SOURCE_BUNDLE,
                tenant_id: TENANT,
                plan_id: source_plan(),
                price_basis: PriceBasis::OwnPrice,
                invoice_itemization: InvoiceItemization::Aggregate,
            },
            stamp(),
        )
        .await
        .expect("make the source a bundle");
    bundles
        .replace_composition(
            &scope(),
            TENANT,
            source_plan(),
            created.revision,
            after_composites.row_version,
            CompositionDraft {
                components: vec![
                    BundleComponentDraft {
                        component_plan_id: COMPONENT_PLAN,
                        included_sku_id: Uuid::from_u128(0x_5c_02),
                        min_qty: Some(1),
                        max_qty: Some(4),
                    },
                    BundleComponentDraft {
                        component_plan_id: COMPONENT_PLAN_B,
                        included_sku_id: Uuid::from_u128(0x_5c_03),
                        min_qty: None,
                        max_qty: None,
                    },
                ],
                rev_share_groups: vec![RevShareGroup {
                    vendor_sku_id: VENDOR_SKU,
                    platform_cut_bp: 1_500,
                    residual_absorber: Absorber::Party(Party::new("vendor").expect("a party name")),
                    parties: vec![PartyShare {
                        party: Party::new("vendor").expect("a party name"),
                        share_bp: 8_500,
                    }],
                }],
            },
            stamp(),
        )
        .await
        .expect("author the source composition");

    PriceRepo::new(provider.clone())
        .create_draft(
            &scope(),
            TENANT,
            NewPriceDraft {
                price_id: SOURCE_ROW,
                scope_key: source_key(),
                content: flat_row(),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the source row");

    common::publish_row_directly(provider, &scope(), SOURCE_ROW).await;
    common::publish_plan_directly(provider, &scope(), source_plan(), created.revision).await;
}

/// **A clone its caller rolls back leaves no row behind — on Postgres.**
///
/// Before D-274 each step was a repository *method* opening its own transaction,
/// so the plan draft and every row under it committed one at a time and the
/// caller had nothing to take back. The sibling `SQLite` case proves the same
/// property and is the one that was probed against that older shape; this one
/// answers the question that probe could not, which is whether the engine agrees.
#[tokio::test]
#[ignore = "requires Postgres; run with --ignored"]
async fn a_clone_its_caller_rolls_back_leaves_no_row_behind() {
    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);
    seed(&provider).await;

    let (_, outcome) = provider
        .db()
        .in_transaction::<(), DomainError, _>(move |txn| {
            Box::pin(async move {
                Box::pin(clone_plan_on(
                    txn,
                    &scope(),
                    TENANT,
                    source_plan(),
                    target_plan(),
                    at(11),
                    stamp(),
                ))
                .await?;
                Err(DomainError::Internal(
                    "the caller fails after the clone".to_owned(),
                ))
            })
        })
        .await;
    // **The injected error, matched.** `outcome.is_err()` alone is satisfied by a
    // refusal arising *inside* `clone_plan_on` -- a seed the fixture forgot, a rule
    // this case is not about -- and every absence assertion below then passes
    // against a database the clone never wrote to. The committed twin one function
    // down is the other half of the pair, and neither is sufficient alone: it says
    // the clone can write, this says the failure under test is the caller's.
    assert!(
        matches!(
            outcome,
            Err(TxError::Domain(DomainError::Internal(ref m)))
                if m == "the caller fails after the clone"
        ),
        "the caller's own failure must be what stands, not a refusal from inside the clone: \
         {outcome:?}"
    );

    let conn = provider.conn().expect("conn");
    assert!(
        plan_repo::load_open_draft(&conn, &scope(), TENANT, target_plan())
            .await
            .expect("read the draft")
            .is_none(),
        "the draft plan the clone created must not survive its caller's rollback"
    );
    assert!(
        price_repo::load_for_plan(
            &conn,
            &scope(),
            TENANT,
            target_plan(),
            &[LifecycleState::Draft],
        )
        .await
        .expect("read the rows")
        .is_empty(),
        "and neither may the price row it copied"
    );
    // The child classes, each named, because a seed that reaches a writer is
    // only evidence if something looks at what that writer wrote.
    assert!(
        plan_shape_repo::load_phase_set(&conn, &scope(), TENANT, target_plan(), 0)
            .await
            .expect("read the phases")
            .is_empty(),
        "nor the phases it remapped"
    );
    assert!(
        plan_shape_repo::load_composite_set(&conn, &scope(), TENANT, target_plan(), 0)
            .await
            .expect("read the composites")
            .is_empty(),
        "nor the composite meters it re-minted"
    );
    assert!(
        plan_shape_repo::load_descriptor(&conn, &scope(), TENANT, target_plan(), 0)
            .await
            .expect("read the descriptor set")
            .is_none(),
        "nor the descriptor set it copied"
    );
    assert!(
        plan_shape_repo::load_addon_rule_set(&conn, &scope(), TENANT, target_plan(), 0)
            .await
            .expect("read the add-on rules")
            .is_empty(),
        "nor the add-on rule it copied"
    );
    assert!(
        bundle_repo::find_by_plan_on(&conn, &scope(), TENANT, target_plan())
            .await
            .expect("read the bundle")
            .is_none(),
        "nor the bundle identity it minted"
    );
    // The composition is asserted separately from the identity because they are
    // written by different functions: `create_on` mints the row above, and
    // `replace_composition_on` fills the three tables under it. A clone that
    // minted the identity and copied an empty composition satisfies the
    // assertion above and not this one.
    let composition = bundle_repo::load_composition_on(&conn, &scope(), TENANT, target_plan(), 0)
        .await
        .expect("read the composition");
    assert!(
        composition.components.is_empty() && composition.rev_share_groups.is_empty(),
        "nor the components and rev-share groups under it"
    );
    // **The audit plane, which is the one table the clone touches that nothing
    // here looked at.** `plan_repo`'s create path appends a `PlanRevision` entry
    // for the draft it mints, and `pricing_audit_log` carries a no-delete and a
    // no-update trigger -- so an audit row that outlived its caller's rollback is
    // the one leak on this list with no remedy at all: nothing in this gear can
    // remove it, and it would name a plan revision that does not exist.
    assert!(
        audit_repo::for_subject(
            &conn,
            &scope(),
            TENANT,
            AuditSubjectKind::PlanRevision,
            &audit_repo::plan_revision_ref(target_plan(), 0),
        )
        .await
        .expect("read the audit trail of the target revision")
        .is_empty(),
        "nor the audit entry it appended for the draft it minted"
    );
}

/// The same clone, committed — so the case above is read as *rollback*, not as a
/// clone that never wrote anything.
///
/// Without this pair the assertion "nothing survived" is satisfied just as well
/// by a clone that failed at its first statement, and a suite that cannot tell
/// those apart is not testing atomicity. Its sibling on `SQLite` is the whole
/// nine-case copy-set suite; here it is one row, which is all the distinction
/// needs.
#[tokio::test]
#[ignore = "requires Postgres; run with --ignored"]
async fn the_same_clone_committed_writes_every_class_the_rollback_removed() {
    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);
    seed(&provider).await;

    let (_, outcome) = provider
        .db()
        .in_transaction::<(), DomainError, _>(move |txn| {
            Box::pin(async move {
                Box::pin(clone_plan_on(
                    txn,
                    &scope(),
                    TENANT,
                    source_plan(),
                    target_plan(),
                    at(11),
                    stamp(),
                ))
                .await
                .map(|_| ())
            })
        })
        .await;
    outcome.expect("the clone commits");

    let conn = provider.conn().expect("conn");
    let revision = plan_repo::load_open_draft(&conn, &scope(), TENANT, target_plan())
        .await
        .expect("read the draft")
        .expect("there is one");
    assert_eq!(revision.cloned_from, Some(source_plan()));
    assert_eq!(
        price_repo::load_for_plan(
            &conn,
            &scope(),
            TENANT,
            target_plan(),
            &[LifecycleState::Draft],
        )
        .await
        .expect("read the rows")
        .len(),
        1,
        "the one published source row came across as a draft"
    );
    // **The same six the rollback case asserts absent.** Together the pair is
    // the evidence that the seed reaches every writer: each class is present
    // here and gone there, so neither reading can be satisfied by a clone that
    // never touched that class at all.
    assert_eq!(
        plan_shape_repo::load_phase_set(&conn, &scope(), TENANT, target_plan(), 0)
            .await
            .expect("read the phases")
            .len(),
        2,
        "the two-link chain came across"
    );
    assert_eq!(
        plan_shape_repo::load_composite_set(&conn, &scope(), TENANT, target_plan(), 0)
            .await
            .expect("read the composites")
            .len(),
        1,
        "the composite meter came across"
    );
    assert!(
        plan_shape_repo::load_descriptor(&conn, &scope(), TENANT, target_plan(), 0)
            .await
            .expect("read the descriptor set")
            .is_some(),
        "the descriptor set came across"
    );
    let rules = plan_shape_repo::load_addon_rule_set(&conn, &scope(), TENANT, target_plan(), 0)
        .await
        .expect("read the add-on rules")
        .len();
    assert_eq!(rules, 1, "the add-on rule came across");
    let bundle = bundle_repo::find_by_plan_on(&conn, &scope(), TENANT, target_plan())
        .await
        .expect("read the bundle")
        .expect("the clone of a bundle is a bundle");
    assert_ne!(
        bundle.bundle_id, SOURCE_BUNDLE,
        "under its own identity (D-269)"
    );
    let composition = bundle_repo::load_composition_on(&conn, &scope(), TENANT, target_plan(), 0)
        .await
        .expect("read the composition");
    assert_eq!(
        composition.components.len(),
        2,
        "both components came across under the new identity"
    );
    assert_eq!(
        composition.rev_share_groups.len(),
        1,
        "and the rev-share group with them"
    );
    assert_eq!(
        composition.components[0].component_plan_id, COMPONENT_PLAN,
        "component_plan_id names another plan and is the one id not re-minted (D-269)"
    );
    // The audit plane's positive half: the rollback case asserts this trail is
    // empty, and an empty trail is only evidence of a rollback if the committed
    // clone writes one. `pricing_audit_log` is append-only by trigger, so this is
    // also the one class whose leak has no remedy.
    assert!(
        !audit_repo::for_subject(
            &conn,
            &scope(),
            TENANT,
            AuditSubjectKind::PlanRevision,
            &audit_repo::plan_revision_ref(target_plan(), 0),
        )
        .await
        .expect("read the audit trail of the target revision")
        .is_empty(),
        "the clone appends an audit entry for the draft it minted"
    );
}
