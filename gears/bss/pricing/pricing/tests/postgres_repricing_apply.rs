//! The mass-repricing apply's own atomicity and idempotency, **on the engine
//! that runs in production** (`design/12-operator-efficiency.md` §3
//! `inst-mr-apply`, `inst-mr-validate-scope`; D-134).
//!
//! # Why this exists as its own suite
//!
//! `tests/sqlite_repricing_apply.rs` drives [`apply_run_in`] over an in-memory
//! `SQLite` database and its own module doc says exactly what that engine
//! cannot speak to: `SQLite` serialises writers and — the fact this suite
//! exists to close — **does not abort a transaction on a failed statement**.
//! `postgres_bulk_commit.rs:10-21` says the identical thing about its own
//! sibling: `SQLite`'s version of the conflict case there "passes whether or
//! not that contract is honoured." `apply_run_in`'s own module doc
//! (`src/infra/repricing.rs`) leans on the same fact from the other side —
//! plan B's aggregate-pass failure has to roll back **every write the row
//! loop just made in that same transaction**, and a `SQLite` connection that
//! keeps going after a failed statement cannot tell a real rollback from a
//! partial commit nobody noticed. This suite is the one place that
//! distinction is load-bearing rather than academic.
//!
//! # What this suite ports, and what it deliberately does not
//!
//! `sqlite_repricing_apply.rs` proves five properties over `apply_run_in`.
//! Two of them are engine-differential and are ported here:
//!
//! * **The per-plan transaction is atomic under a real aborting engine.** A
//!   run spanning two plans where the second plan's aggregate pass fails:
//!   plan A applies whole, plan B applies none of its rows, and every one of
//!   plan B's journal rows reads `failed` with the one shared reason
//!   `apply_by_plan`'s catch-all writes. On `SQLite` a failed statement
//!   inside plan B's transaction does not necessarily undo what came before
//!   it in the same transaction; on Postgres it does, unconditionally — this
//!   is the property `SQLite` cannot refute, only fail to contradict.
//! * **Re-run idempotency over a journal a real rollback left behind.** A
//!   second `apply_run_in` call over the same run — now carrying both
//!   `applied` and `failed` rows, the `failed` half having survived a genuine
//!   engine-level rollback rather than a `SQLite` connection that merely kept
//!   going — applies nothing twice and answers the identical [`RunOutcome`].
//!
//! The other three are **not** ported:
//!
//! * `inst-mp-grandfathered` clauses 1 and 2 (the selector's structural
//!   exclusion, and the per-row refusal of an explicitly-selected
//!   grandfathered row) are pure application logic — `RunSelector::admits_grandfathered`
//!   and `price_repo::refuse_unsupersedable_class` read no engine-specific
//!   behaviour at all, and `sqlite_repricing_apply.rs`'s three cases already
//!   prove them. Porting them here would be coverage of the mirror wearing a
//!   different connection string, not coverage of the engine.
//! * The bulk lock's `Drop`-guard release on a genuinely cancelled future
//!   (`sqlite_repricing_apply.rs`'s `a_future_dropped_mid_apply_releases_its_bulk_lock_and_leaves_the_run_committing`)
//!   is [`Drop`]'s own language guarantee, which fires identically regardless
//!   of what pool sits under it — `RunLockGuard`'s own doc is explicit that
//!   what it promises and does not promise is about Tokio and the process,
//!   not about the database engine. What *would* be engine-specific — whether
//!   an aborted-but-never-explicitly-rolled-back transaction's connection is
//!   returned to a real two-connection pool cleanly enough for the guard's own
//!   detached recovery task to acquire one — is a real question this suite
//!   cannot safely answer without being able to run it: getting a timing
//!   budget or a polling loop wrong against a shared two-connection Postgres
//!   pool (`pg_support`'s own cap, chosen because one server now carries every
//!   suite's pools at once) risks a flake or a deadlock in a job that runs on
//!   every merge, for a property that is not the one this task was scoped to
//!   close. Left for a task that can execute it.
//!
//! # Why the seed goes through the real publish writers, not a bypass
//!
//! `postgres_clone_atomicity.rs`'s own seed publishes past the engine
//! (`common::publish_row_directly`/`publish_plan_directly`) for a stated
//! reason: that suite's subject is the clone, and a fixture routed through
//! the publish pipeline would make a pipeline failure read as a clone
//! failure. That reason does not hold here. `apply_run_in`'s own aggregate
//! pass reads the plan's *current* revision and its rows' lifecycle state
//! back through `plan_repo::load_current`/`price_repo::load_for_plan` —
//! exactly what a real publish leaves behind — and `plan_repo::publish_revision`/
//! `price_repo::publish_rows` are the same generic, engine-dispatched
//! functions `sqlite_repricing_apply.rs` already seeds through. Using them
//! here rather than a raw `UPDATE` keeps this suite's only engine-specific
//! surface exactly where the module doc above says it is: inside
//! `apply_run_in` itself, not in how the fixture got its rows to `published`.
//!
//! Run with:
//! `cargo test -p cf-gears-bss-pricing --test postgres_repricing_apply -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod pg_support;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use toolkit_canonical_errors::CanonicalError;

use async_trait::async_trait;
use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::bulk::{BulkKind, BulkState, JournalState};
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::contracts::{BillingAnchorPolicy, ProrationBasis, ProrationContract};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::overlay::{Adjustment, Magnitude};
use bss_pricing::domain::plan_shape::{
    BillingCycle, DescriptorSet, Frequency, PhaseKind, PlanPhase,
};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::repricing::apply_run_in;
use bss_pricing::infra::storage::repo::repricing_journal_repo::NewJournalRow;
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, NewBulkOperation, NewPlanDraft, NewPriceDraft, PlanRepo, PlanShapeRepo,
    PolicyObjectRepo, PriceRepo, bulk_repo, repricing_journal_repo,
};
use bss_pricing_sdk::catalog_version_registry::{CatalogVersionRegistryV1, PendingVersionRef};
use time::OffsetDateTime;

use bss_pricing::domain::instant::format_rfc3339;
use pg_support::Pg;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// The registry double — this suite's own, per `sqlite_publish_commit.rs`'s
// convention that no double is shared across binaries.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RegistryDouble {
    issued: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl CatalogVersionRegistryV1 for RegistryDouble {
    async fn request_version(
        &self,
        _ctx: &SecurityContext,
        request_id: &str,
    ) -> Result<PendingVersionRef, CanonicalError> {
        let mut issued = self.issued.lock().expect("no panics in the double");
        let next = issued.len();
        let pending = issued
            .entry(request_id.to_owned())
            .or_insert_with(|| format!("pend-{next}"))
            .clone();
        Ok(PendingVersionRef {
            request_id: request_id.to_owned(),
            pending_ref: pending,
        })
    }

    async fn committed_version(
        &self,
        _ctx: &SecurityContext,
        _pending_ref: &str,
    ) -> Result<Option<bss_pricing_sdk::catalog_version::CatalogVersion>, CanonicalError> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// The harness.
// ---------------------------------------------------------------------------

const TENANT: Uuid = Uuid::from_u128(0x7e_81);
const ACTOR: Uuid = Uuid::from_u128(0xac_80);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_81);

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(ACTOR)
        .subject_tenant_id(TENANT)
        .build()
        .expect("a subject and a tenant are all a context needs")
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 11, hour, 0, 0)
}

/// Far enough out that no wall clock reaches it, and clear of the batching
/// delay floor `ChangeoverMoment::Commit` holds the apply to —
/// `tests/sqlite_repricing_apply.rs`'s own constant, for the identical
/// reason.
fn changeover() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 20, 0, 0, 0)
}

/// One test's own database, migrated, plus the repositories and the registry
/// double `apply_run_in` needs.
///
/// A single `DBProvider` — a single, shared, two-connection pool
/// (`pg_support::Pg::db`) — for every repository here **and** for
/// `apply_run_in` itself, exactly as `postgres_clone_atomicity.rs`'s own
/// harness shares one provider across `PlanRepo`/`PlanShapeRepo`/`PriceRepo`/
/// `BundleRepo` and the writer under test. Every use below is a sequential
/// `await`, never a concurrent one, so at most one connection is ever checked
/// out at a time during seeding, and `apply_run_in`'s own internal mix of one
/// autocommit connection plus one per-plan transaction (never more than one
/// plan's transaction open at once — the per-plan loop is sequential) fits
/// inside the cap with room to spare.
struct Harness {
    provider: DBProvider<DbError>,
    plans: PlanRepo,
    shapes: PlanShapeRepo,
    prices: PriceRepo,
    policies: PolicyObjectRepo,
    registry: Arc<RegistryDouble>,
    scope: AccessScope,
}

async fn harness() -> Harness {
    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);
    common::declare_fixture_regions(&provider, TENANT).await;
    Harness {
        provider: provider.clone(),
        plans: PlanRepo::new(provider.clone()),
        shapes: PlanShapeRepo::new(provider.clone()),
        prices: PriceRepo::new(provider.clone()),
        policies: PolicyObjectRepo::new(
            provider.clone(),
            &bss_pricing::config::LimitsConfig::default(),
        ),
        registry: Arc::new(RegistryDouble::default()),
        scope: AccessScope::for_tenant(TENANT),
    }
}

/// A row the whole aggregate rule set passes: recurring, flat, tax-exclusive,
/// carrying the billing timing, proration contract, rounding policy and tax
/// category that `inst-pi-required`, `ROUNDING_POLICY_UNRESOLVED` and
/// `TAX_BASIS_INCOMPLETE` each demand of one — `sqlite_repricing_apply.rs`'s own
/// fixture, unchanged, including the category it gained with H14 of the 2026-08-19
/// review (see that file's copy for why).
fn publishable_row(amount_minor: i64) -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(amount_minor).expect("a non-negative amount"));
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: Some("standard".to_owned()),
        billing_timing: Some("advance".to_owned()),
        proration_contract: Some(ProrationContract {
            billing_anchor_policy: BillingAnchorPolicy::CalendarMonth,
            proration_basis: ProrationBasis::CalendarDaysActual,
            credit_on_downgrade: false,
        }),
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

/// A plan the whole aggregate rule set passes: one evergreen terminal phase, a
/// complete descriptor set, a tier and a frequency — built at the repository
/// seam, then published through the real writer (see the module doc for why).
async fn seed_plan(h: &Harness, plan_id: Uuid, phase_id: Uuid) {
    let plan = PlanId::new(plan_id);
    let created = h
        .plans
        .create_draft(
            &h.scope,
            NewPlanDraft {
                plan_name: None,
                plan_id: plan,
                tenant_id: TENANT,
                created_by: ACTOR,
                created_at_utc: at(10),
                sku_id: Some(Uuid::from_u128(0x5_c1)),
                plan_tier: Some("gold".to_owned()),
                billing_cycle: Some(BillingCycle::Recurring),
                frequency: Some(Frequency::Monthly),
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
        .expect("create the draft plan");
    let after_phases = h
        .shapes
        .replace_phases(
            &h.scope,
            TENANT,
            plan,
            created.revision,
            created.row_version,
            vec![PlanPhase {
                phase_id: PhaseId::new(phase_id),
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
    h.shapes
        .set_descriptor_set(
            &h.scope,
            TENANT,
            plan,
            created.revision,
            after_phases.row_version,
            DescriptorSet {
                invoice_line_template: Some("{plan}".to_owned()),
                gl_code: Some("4000".to_owned()),
                itemization_rule: Some("per_charge".to_owned()),
                additional: std::collections::BTreeMap::new(),
            },
            stamp(),
        )
        .await
        .expect("attach the descriptor set");
    publish_plan(h, plan, created.revision).await;
}

async fn publish_plan(h: &Harness, plan: PlanId, revision: u64) {
    let current = h
        .plans
        .find_revision(&h.scope, TENANT, plan, revision)
        .await
        .expect("read the revision")
        .expect("the revision exists");
    let scope = h.scope.clone();
    let (_, outcome) = h
        .provider
        .db()
        .in_transaction::<_, bss_pricing::infra::storage::RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::plan_repo::publish_revision(
                    txn,
                    &scope,
                    TENANT,
                    plan,
                    revision,
                    current.row_version,
                )
                .await
            })
        })
        .await;
    outcome.expect("publish the seeded plan revision");
}

fn scope_key(plan: PlanId, phase: Uuid, region: &str) -> ScopeKey {
    ScopeKey::new(
        plan,
        CurrencyCode::new("USD").expect("currency"),
        Region::new(region).expect("region"),
        PhaseId::new(phase),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("scope key")
}

/// Author and publish one row, with a scheduled coverage window
/// (`inst-wc-required`) — the shape every row this suite selects for
/// repricing needs.
async fn seed_published_row(
    h: &Harness,
    plan: PlanId,
    phase: Uuid,
    region: &str,
    amount_minor: i64,
) -> Uuid {
    let price_id = Uuid::now_v7();
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key: scope_key(plan, phase, region),
                content: publishable_row(amount_minor),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the price row");
    common::schedule_coverage_window(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        price_id,
        stamp(),
    )
    .await;
    let scope = h.scope.clone();
    let (_, outcome) = h
        .provider
        .db()
        .in_transaction::<_, bss_pricing::infra::storage::RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::price_repo::publish_rows(
                    txn,
                    &scope,
                    TENANT,
                    plan,
                    &[(price_id, RowVersion::new(0))],
                    &bss_pricing::domain::tax_display::RegionTaxReadiness::empty(),
                    None,
                )
                .await
            })
        })
        .await;
    outcome.expect("publish the seeded price row");
    price_id
}

/// A **stray draft** row: authored, never published, no window scheduled —
/// the shape `apply_run_in`'s own module doc names as the mechanism that
/// fails a plan's aggregate pass through `WINDOW_COVERAGE_MISSING`. On a
/// different key from every published row this suite seeds, so it collides
/// with nothing.
///
/// The region is `us-east` and has to be **declared**. It was `apac`, which
/// `common::declare_fixture_regions` does not declare (its set is
/// `eu`/`EU`/`us`/`US`/`DE`/`us-east`), so plan B's aggregate pass failed on
/// `REGION_UNKNOWN` as well — `RegionsDeclared::evaluate` walks every candidate row
/// — and the module doc's load-bearing explanation was not what the fixture
/// isolated. A maintainer who "fixed" the stray draft by scheduling a window on it
/// would have found the plan still failing for a reason the doc never mentions.
/// `sqlite_repricing_apply::seed_stray_draft` is the twin and states the same
/// thing; the two are one fixture on two engines and must not diverge on it.
async fn seed_stray_draft(h: &Harness, plan: PlanId, phase: Uuid) {
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: Uuid::now_v7(),
                scope_key: scope_key(plan, phase, "us-east"),
                content: publishable_row(5_000),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the stray draft");
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: at(10),
        correlation_id: CORRELATION,
    }
}

fn apply_stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: at(12),
        correlation_id: CORRELATION,
    }
}

/// `api::rest::repricing_runs::frozen_report`'s exact wire shape — this
/// suite's only way to hand [`apply_run_in`] an adjustment and a changeover,
/// since it takes neither directly and reads both off the run's own stored
/// report.
///
/// **The two kind tokens are rendered by the producer, not typed.** They were
/// JSON literals until 2026-08-18 (review Z2-11): production writes them through
/// `Adjustment::kind()` / `::magnitude_kind()` and reads them back through
/// `adjustment_of`, so a rename moved producer and consumer together and left
/// this fixture asserting against the old spelling — green, and testing nothing
/// about the round trip it exists to drive.
fn report() -> serde_json::Value {
    let adjustment = Adjustment::Discount(Magnitude::PercentBp(500));
    serde_json::json!({
        "selector": serde_json::Value::Null,
        "adjustment": {
            "adjustment_kind": adjustment.kind().as_str(),
            "magnitude_kind": adjustment.magnitude_kind().as_str(),
            "adjustment_value": adjustment.percent_bp(),
            "amounts": {},
        },
        "changeover": format_rfc3339(changeover()),
        "selected": 0,
    })
}

/// Open a run already `committing`, with `price_ids` frozen into its journal
/// `pending` — `open_repricing_run`'s own two writes, built directly rather
/// than through HTTP: this suite's subject is the apply, not the freeze.
async fn open_committing_run(h: &Harness, price_ids: &[Uuid]) -> Uuid {
    let operation_id = Uuid::now_v7();
    let conn = h.provider.conn().expect("conn");
    bulk_repo::open(
        &conn,
        &h.scope,
        NewBulkOperation {
            operation_id,
            tenant_id: TENANT,
            kind: BulkKind::Repricing,
            client_key: operation_id.to_string(),
            request_hash: IdempotencyGate::payload_hash(&operation_id.to_string()),
            report: report(),
            submitted_by: ACTOR,
            submitted_at: at(11),
        },
    )
    .await
    .expect("open the run");
    let rows: Vec<NewJournalRow> = price_ids
        .iter()
        .map(|&price_id| NewJournalRow {
            run_id: operation_id,
            price_id,
            tenant_id: TENANT,
        })
        .collect();
    repricing_journal_repo::open_rows(&conn, &h.scope, &rows)
        .await
        .expect("freeze the journal");
    bulk_repo::advance(
        &conn,
        &h.scope,
        TENANT,
        operation_id,
        BulkState::Validating,
        BulkState::Committing,
        report(),
        at(11),
    )
    .await
    .expect("enter committing");
    operation_id
}

async fn journal_state(
    h: &Harness,
    run_id: Uuid,
    price_id: Uuid,
) -> (JournalState, Option<String>, Option<Uuid>) {
    let rows = repricing_journal_repo::list_for_run(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        run_id,
    )
    .await
    .expect("read the journal");
    let row = rows
        .into_iter()
        .find(|row| row.price_id == price_id)
        .expect("the row is on this run's journal");
    (row.state, row.failure_reason, row.applied_price_id)
}

// ---------------------------------------------------------------------------
// The atomicity and idempotency cases.
// ---------------------------------------------------------------------------

/// **The property this whole suite exists for.** Plan A: one clean row,
/// applies whole. Plan B: two clean, selected rows, plus a stray draft on a
/// third, unselected key that fails plan B's aggregate pass
/// (`WINDOW_COVERAGE_MISSING`) — the identical mechanism
/// `sqlite_repricing_apply.rs`'s own module doc explains at length, ported
/// unchanged because the mechanism itself is not what is under test here.
/// What is under test is what a **real** engine does with the write plan B's
/// row loop already made, inside the same transaction, once that plan's
/// aggregate pass returns `Err`: Postgres aborts the transaction outright,
/// and this suite reads that rollback back rather than assuming it.
///
/// The redrive at the end proves the second property this suite owes: a
/// second `apply_run_in` call over a journal now carrying both `applied` and
/// `failed` rows — the `failed` half decided by a real rollback, not a
/// `SQLite` connection that merely kept going — applies nothing twice and
/// answers the identical [`RunOutcome`].
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_plan_whose_aggregate_pass_fails_applies_none_of_that_plans_rows_and_a_redrive_applies_nothing_twice()
 {
    let h = harness().await;

    // Plan A: one clean row, nothing else on it. Applies whole.
    let plan_a = Uuid::now_v7();
    let phase_a = Uuid::now_v7();
    seed_plan(&h, plan_a, phase_a).await;
    let row_a = seed_published_row(&h, PlanId::new(plan_a), phase_a, "eu", 9_900).await;

    // Plan B: two clean, selected rows, plus a stray draft on a third key —
    // never selected, never touched, and the reason plan B's aggregate pass
    // fails (see the module doc).
    let plan_b = Uuid::now_v7();
    let phase_b = Uuid::now_v7();
    seed_plan(&h, plan_b, phase_b).await;
    let row_b1 = seed_published_row(&h, PlanId::new(plan_b), phase_b, "eu", 10_000).await;
    let row_b2 = seed_published_row(&h, PlanId::new(plan_b), phase_b, "us", 12_000).await;
    seed_stray_draft(&h, PlanId::new(plan_b), phase_b).await;

    let run_id = open_committing_run(&h, &[row_a, row_b1, row_b2]).await;

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await
    .expect("apply_run_in itself does not fail - only a plan's own rows do");

    assert_eq!(outcome.applied, 1, "plan A's one row: {outcome:?}");
    assert_eq!(
        outcome.failed, 2,
        "a partial plan is the one outcome D-134 forbids - plan B's whole selection fails: \
         {outcome:?}"
    );

    let (state_a, reason_a, applied_price_id_a) = journal_state(&h, run_id, row_a).await;
    assert_eq!(state_a, JournalState::Applied, "plan A applies whole");
    assert!(reason_a.is_none());
    assert!(
        applied_price_id_a.is_some(),
        "a real successor exists for row A"
    );

    let (state_b1, reason_b1, applied_b1) = journal_state(&h, run_id, row_b1).await;
    let (state_b2, reason_b2, applied_b2) = journal_state(&h, run_id, row_b2).await;
    assert_eq!(
        state_b1,
        JournalState::Failed,
        "plan B's aggregate pass refused it: no partial plan"
    );
    assert_eq!(state_b2, JournalState::Failed);
    assert!(
        applied_b1.is_none(),
        "plan B applied nothing, not even partially"
    );
    assert!(applied_b2.is_none());

    let reasons: BTreeSet<String> = [reason_b1.clone(), reason_b2]
        .into_iter()
        .map(|reason| reason.expect("a failed row carries a reason"))
        .collect();
    assert_eq!(
        reasons.len(),
        1,
        "every row of the plan fails with the shared reason: {reasons:?}"
    );
    let reason = reasons.into_iter().next().expect("exactly one");
    assert!(
        reason.contains("WINDOW_COVERAGE_MISSING"),
        "the aggregate-only rule the stray draft trips: {reason}"
    );
    assert!(
        !reason.contains("REGION_UNKNOWN"),
        "and **only** that rule: the stray draft's region was `apac`, which \
         `declare_fixture_regions` does not declare, so `RegionsDeclared` refused the plan \
         for a second reason and this case was green about a mechanism it was not \
         isolating: {reason}"
    );

    // Plan A's row genuinely left the published plane and a real successor
    // stands on its key - the read that would catch a rollback silently
    // reaching plan A's writes too.
    let clean_plan_rows = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        PlanId::new(plan_a),
        &[
            bss_pricing::domain::lifecycle::LifecycleState::Published,
            bss_pricing::domain::lifecycle::LifecycleState::Superseded,
        ],
    )
    .await
    .expect("read plan A's rows");
    assert_eq!(
        clean_plan_rows
            .iter()
            .filter(
                |r| r.lifecycle_state == bss_pricing::domain::lifecycle::LifecycleState::Published
            )
            .count(),
        1,
        "plan A holds exactly one published row - the successor: {clean_plan_rows:?}"
    );
    assert_eq!(
        clean_plan_rows
            .iter()
            .filter(
                |r| r.lifecycle_state == bss_pricing::domain::lifecycle::LifecycleState::Superseded
            )
            .count(),
        1,
        "and the predecessor, superseded rather than gone: {clean_plan_rows:?}"
    );

    // **The one assertion this whole suite exists for.** Plan B's two
    // selected rows are untouched: still published, under their original
    // ids, nothing superseded - proof that Postgres rolled the whole
    // transaction back rather than leaving the row loop's writes standing
    // beside the aggregate pass's refusal. `SQLite` cannot fail this
    // assertion even given a defect that skips the rollback outright
    // (`postgres_bulk_commit.rs:10-21`'s own point); Postgres can, and this
    // is the read that would catch it.
    let broken_plan_rows = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        PlanId::new(plan_b),
        &[bss_pricing::domain::lifecycle::LifecycleState::Published],
    )
    .await
    .expect("read plan B's published rows");
    let broken_plan_published: BTreeSet<Uuid> =
        broken_plan_rows.iter().map(|r| r.price_id).collect();
    assert!(
        broken_plan_published.contains(&row_b1) && broken_plan_published.contains(&row_b2),
        "plan B's rows still stand under their own ids - the transaction rolled back whole: \
         {broken_plan_rows:?}"
    );
    assert_eq!(
        broken_plan_rows.len(),
        2,
        "and nothing else was left behind by a partial commit: {broken_plan_rows:?}"
    );

    // ------------------------------------------------------------------
    // The idempotency case, over this exact run: applied and failed rows
    // both already decided, the failed half by a real rollback.
    // ------------------------------------------------------------------
    let redrive = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await
    .expect("a redrive over an already-decided journal does not fail");

    assert_eq!(
        redrive, outcome,
        "a re-run over a journal containing applied and failed rows applies nothing twice and \
         answers the same outcome"
    );
    let (_, _, applied_price_id_a_again) = journal_state(&h, run_id, row_a).await;
    assert_eq!(
        applied_price_id_a_again, applied_price_id_a,
        "the same successor, not a second one minted by a double-apply"
    );
    let clean_plan_rows_again = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        PlanId::new(plan_a),
        &[bss_pricing::domain::lifecycle::LifecycleState::Published],
    )
    .await
    .expect("read plan A's rows again");
    assert_eq!(
        clean_plan_rows_again.len(),
        1,
        "still exactly one published row on plan A's key - a double-apply would mint a second: \
         {clean_plan_rows_again:?}"
    );
    let (state_b1_again, reason_b1_again, _) = journal_state(&h, run_id, row_b1).await;
    assert_eq!(
        state_b1_again,
        JournalState::Failed,
        "plan B's row is still failed, not silently retried into applied"
    );
    assert_eq!(
        reason_b1_again, reason_b1,
        "and it carries the same reason it did the first time, not a fresh evaluation"
    );

    let stored = bulk_repo::read(&h.provider.conn().expect("conn"), &h.scope, TENANT, run_id)
        .await
        .expect("read the run")
        .expect("the run exists");
    assert_eq!(
        stored.state,
        BulkState::CompletedWithConflicts,
        "one plan applied, one failed - a success with conflicts (`inst-bk-phase2`'s reading, \
         carried over from bulk import to repricing), and unchanged by the redrive: {stored:?}"
    );
}
