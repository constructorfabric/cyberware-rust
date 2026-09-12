//! `TaxonomyRepo` against a real database — the read and write surface the four
//! taxonomies did not have (`design/04-currency-tax.md` §3, §5, §6).
//!
//! `sqlite_taxonomy_store.rs` proves the **schema**: the composite key, the state
//! enumeration, the blank-value refusal, and the two `tax_*` columns §6 puts on
//! the region taxonomy alone. This suite proves the **repository** — that a `PUT`
//! is a whole-set replacement, that absence retires rather than deletes, that
//! `inst-tx-mutation`'s guard actually consults both referencing planes, and that
//! C4's readiness lookup distinguishes an undeclared region from a declared one
//! with no default category.
//!
//! Those are different claims and neither implies the other. A repository that
//! writes only valid values catches a constraint that got narrower and never one
//! that stopped refusing; a schema suite catches a `CHECK` that changed and never
//! a guard the repository forgot to call.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::SecureUpdateExt;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

use bss_pricing::infra::storage::entity::{
    audit_log, plan, plan_descriptor_set, price, price_overlay,
};
use bss_pricing::infra::storage::migrations::Migrator;

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::overlay::{ScopeClass, ScopeValue};
use bss_pricing::domain::scope_key::Region;
use bss_pricing::domain::taxonomy::{
    RegionTaxMarkers, SEEDED_REGION, TAXONOMY_VALUE_IN_USE, TaxonomyClass, TaxonomyEntry,
    TaxonomyState, TaxonomyValueChange, TaxonomyValuePatch, TaxonomyValueProposal, seeded_region,
    tag_of,
};
use bss_pricing::infra::approval::ApprovalService;
use bss_pricing::infra::storage::repo::taxonomy_repo::{
    Replaced, TaxonomyRepo, active_gl_codes, active_regions, customer_group_tag_of, gl_code_tag_of,
    references_to, region_readiness, region_readiness_map, rounding_policy_tag_of,
    write_value_patch,
};
use time::OffsetDateTime;

const TENANT: Uuid = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x9999_9999_9999_9999_9999_9999_9999_9999);

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 6, 12, 0, 0)
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: Uuid::from_u128(0xac_10),
        recorded_at: now(),
        correlation_id: Uuid::from_u128(0xc0_11),
    }
}

fn value(raw: &str) -> ScopeValue {
    ScopeValue::new(raw).expect("non-blank")
}

fn entry(value: &str, state: TaxonomyState) -> TaxonomyEntry {
    TaxonomyEntry {
        value: ScopeValue::new(value).expect("non-blank"),
        display_name: format!("label for {value}"),
        state,
        tax: None,
    }
}

fn region_entry(value: &str, category: Option<&str>, rate_present: bool) -> TaxonomyEntry {
    TaxonomyEntry {
        value: ScopeValue::new(value).expect("non-blank"),
        display_name: format!("label for {value}"),
        state: TaxonomyState::Active,
        tax: Some(RegionTaxMarkers {
            tax_category: category.map(ToOwned::to_owned),
            tax_rate_present: rate_present,
        }),
    }
}

fn values(entries: &[TaxonomyEntry]) -> Vec<(&str, TaxonomyState)> {
    entries
        .iter()
        .map(|e| (e.value.as_str(), e.state))
        .collect()
}

/// `replace` under the tag the store currently renders.
///
/// Most cases in this file are not about the precondition — they assert what a
/// `PUT` *does* — and reading the tag immediately before the call is exactly what
/// a well-behaved caller does. The two cases that *are* about the precondition
/// call `replace` directly with a tag they chose.
///
/// Returns the `Result` rather than unwrapping, so a call site can still say what
/// it expects of a failure.
async fn replace_now(
    repo: &TaxonomyRepo,
    scope: &AccessScope,
    class: TaxonomyClass,
    entries: Vec<TaxonomyEntry>,
) -> Result<Replaced, bss_pricing::infra::storage::RepoError> {
    let held = repo.list(scope, TENANT, class).await.expect("read back");
    repo.replace(
        scope,
        TENANT,
        class,
        entries,
        &tag_of(class, &held),
        stamp(),
    )
    .await
}

/// A migrated in-memory database, as every repository suite in this crate builds one.
async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

/// The repository, the scope every call takes, and the provider the seeds and
/// the direct reads go through.
///
/// The provider is handed back rather than held in a static for
/// `sqlite_overlay_repo`'s reason: these cases run in one process and a shared
/// database would let one case's seed decide another's verdict.
async fn harness() -> (TaxonomyRepo, AccessScope, DBProvider<DbError>) {
    let provider = provider().await;
    (
        TaxonomyRepo::new(provider.clone()),
        AccessScope::allow_all(),
        provider,
    )
}

/// How many audit records name one taxonomy.
async fn audit_records_for(provider: &DBProvider<DbError>, subject_ref: &str) -> u64 {
    let conn = provider.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(audit_log::Column::TenantId.eq(TENANT))
                .add(audit_log::Column::SubjectRef.eq(subject_ref)),
        )
        .count(&conn)
        .await
        .expect("count audit rows")
}

// ---------------------------------------------------------------------------
// The `PUT` is the whole set.
// ---------------------------------------------------------------------------

/// The first `PUT` on an empty taxonomy declares its values.
#[tokio::test]
async fn a_first_put_declares_the_whole_set() {
    let (repo, scope, _provider) = harness().await;

    let result = replace_now(
        &repo,
        &scope,
        TaxonomyClass::Brand,
        vec![
            entry("acme", TaxonomyState::Active),
            entry("zenith", TaxonomyState::Active),
        ],
    )
    .await
    .expect("the first put lands");

    assert!(result.report.is_publishable(), "nothing to guard yet");
    assert_eq!(
        values(&result.entries),
        [
            ("acme", TaxonomyState::Active),
            ("zenith", TaxonomyState::Active)
        ],
        "and the response is the taxonomy as it now stands, ordered by value"
    );
}

/// A value the body leaves out is **retired**, not deleted.
///
/// The distinction is the module's central decision. Deleting would break the
/// guard's whole purpose — a value a published row names has to keep existing,
/// because the row keeps naming it — and it would make re-activation, which §6
/// explicitly permits, reachable only by re-inventing the string.
#[tokio::test]
async fn a_value_the_body_omits_is_retired_and_still_readable() {
    let (repo, scope, _provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Brand,
        vec![
            entry("acme", TaxonomyState::Active),
            entry("zenith", TaxonomyState::Active),
        ],
    )
    .await
    .expect("seed");

    let result = replace_now(
        &repo,
        &scope,
        TaxonomyClass::Brand,
        vec![entry("acme", TaxonomyState::Active)],
    )
    .await
    .expect("the second put lands");

    assert_eq!(
        values(&result.entries),
        [
            ("acme", TaxonomyState::Active),
            ("zenith", TaxonomyState::Retired)
        ],
        "the omitted value is retired and still present"
    );
}

/// §6: *"a `PUT` re-adding an existing retired value re-activates it"*.
#[tokio::test]
async fn re_adding_a_retired_value_re_activates_it() {
    let (repo, scope, _provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Partner,
        vec![entry("reseller-a", TaxonomyState::Active)],
    )
    .await
    .expect("seed");
    replace_now(&repo, &scope, TaxonomyClass::Partner, vec![])
        .await
        .expect("retire it");

    let result = replace_now(
        &repo,
        &scope,
        TaxonomyClass::Partner,
        vec![entry("reseller-a", TaxonomyState::Active)],
    )
    .await
    .expect("re-activate");

    assert_eq!(
        values(&result.entries),
        [("reseller-a", TaxonomyState::Active)],
        "retired -> active is a legal audited move (section 6)"
    );
}

/// An explicit `state: retired` in the body writes the retired state.
///
/// **Named for what it tests.** It was called
/// `an_explicit_retirement_and_an_omission_are_the_same_act`, which is a claim
/// about the *guard*, and this case never reaches the guard — the value is
/// unreferenced, so the write goes through either way. The claim is held by
/// `an_explicit_retirement_of_a_referenced_value_is_refused_too`, which a probe
/// forced into existence.
#[tokio::test]
async fn an_explicit_retirement_writes_the_retired_state() {
    let (repo, scope, _provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::OrgTier,
        vec![entry("gold", TaxonomyState::Active)],
    )
    .await
    .expect("seed");

    let result = replace_now(
        &repo,
        &scope,
        TaxonomyClass::OrgTier,
        vec![entry("gold", TaxonomyState::Retired)],
    )
    .await
    .expect("explicit retirement");

    assert_eq!(values(&result.entries), [("gold", TaxonomyState::Retired)]);
}

/// The guard reaches the **explicit** spelling of a retirement too.
///
/// Added because a probe found nothing to redden. Ignoring `state: retired` in
/// the body — while still writing it — left every case in this suite green: the
/// omission case covers the guard, and the explicit case only checked that the
/// state landed. So the module doc's claim that "an operator must not be able to
/// slip past the guard by choosing the other spelling" was true of the code and
/// held by nothing, which is the same defect as a rule with no rule.
#[tokio::test]
async fn an_explicit_retirement_of_a_referenced_value_is_refused_too() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Brand,
        vec![entry("in-use", TaxonomyState::Active)],
    )
    .await
    .expect("seed");
    publish_overlay_scoped(
        &provider,
        0x0e_1c,
        TaxonomyClass::Brand,
        "in-use",
        "published",
    )
    .await;

    let result = replace_now(
        &repo,
        &scope,
        TaxonomyClass::Brand,
        vec![entry("in-use", TaxonomyState::Retired)],
    )
    .await
    .expect("refused, not errored");

    assert_eq!(
        result
            .report
            .violations
            .iter()
            .map(|v| v.code.as_str())
            .collect::<Vec<_>>(),
        [TAXONOMY_VALUE_IN_USE],
        "an explicit retirement is a retirement"
    );
    assert_eq!(
        values(&result.entries),
        [("in-use", TaxonomyState::Active)],
        "and nothing was written"
    );
}

/// The four taxonomies are independent stores.
///
/// A `PUT` on one class must not retire another's values — the failure mode a
/// shared-table implementation would have, and the reason each class is its own
/// entity here.
#[tokio::test]
async fn a_put_on_one_class_leaves_the_other_three_alone() {
    let (repo, scope, _provider) = harness().await;
    for class in TaxonomyClass::ALL {
        replace_now(
            &repo,
            &scope,
            *class,
            vec![entry("shared-value", TaxonomyState::Active)],
        )
        .await
        .expect("seed each");
    }

    replace_now(&repo, &scope, TaxonomyClass::Brand, vec![])
        .await
        .expect("clear the brand taxonomy");

    for class in TaxonomyClass::ALL
        .iter()
        .filter(|c| **c != TaxonomyClass::Brand)
    {
        let held = repo.list(&scope, TENANT, *class).await.expect("read back");
        // The region universe also carries the seed the first write materialised
        // and the whole-set replace then retired (D-354).
        let expected = if *class == TaxonomyClass::Region {
            vec![
                (SEEDED_REGION, TaxonomyState::Retired),
                ("shared-value", TaxonomyState::Active),
            ]
        } else {
            vec![("shared-value", TaxonomyState::Active)]
        };
        assert_eq!(
            values(&held),
            expected,
            "{class} must be untouched by a brand PUT"
        );
    }
}

/// SQL-level BOLA: a foreign tenant's taxonomy reads empty.
#[tokio::test]
async fn a_foreign_tenants_taxonomy_is_invisible() {
    let (repo, scope, _provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Brand,
        vec![entry("acme", TaxonomyState::Active)],
    )
    .await
    .expect("seed");

    // **Two exclusions, and they are different mechanisms.** Reading under
    // `allow_all` exercises the explicit `tenant_id` argument alone; a caller
    // scoped to another tenant is what exercises the compiled scope's own SQL
    // predicate, which is the half a label of "SQL-level BOLA" is about and which
    // no case in this file drove.
    let by_argument = repo
        .list(
            &AccessScope::allow_all(),
            OTHER_TENANT,
            TaxonomyClass::Brand,
        )
        .await
        .expect("read");
    assert!(
        by_argument.is_empty(),
        "another tenant's values are not visible to the argument"
    );

    // **Crossed deliberately**: the scope is the intruder's, the argument names the
    // *owner*. Passing `OTHER_TENANT` to both leaves the explicit argument doing the
    // excluding, so the compiled scope's SQL predicate could be deleted whole and
    // the case would stay green - the opposite of what a label of "SQL-level BOLA"
    // means.
    let by_scope = repo
        .list(
            &AccessScope::for_tenant(OTHER_TENANT),
            TENANT,
            TaxonomyClass::Brand,
        )
        .await
        .expect("read");
    assert!(
        by_scope.is_empty(),
        "nor to a caller scoped elsewhere reaching for the owner's rows by name"
    );

    // The positive control: without it both refusals above are satisfied by a
    // `list` that answers empty to everybody.
    assert_eq!(
        repo.list(
            &AccessScope::for_tenant(TENANT),
            TENANT,
            TaxonomyClass::Brand
        )
        .await
        .expect("read")
        .len(),
        1,
        "and the owner, scoped to itself, still sees its own value"
    );
}

// ---------------------------------------------------------------------------
// The `If-Match` premise, tested where it is enforced (T-7).
// ---------------------------------------------------------------------------

/// The **direct** value commit refuses a value the store has moved past, inside
/// its own transaction — D-355's arm of the property the test below proves for
/// `replace`.
///
/// The handler compares `tag_of_value` against `If-Match` on a plain connection
/// and then hands the decision to a transaction that races it, and
/// `update_entry` is an unconditional `UPDATE` over tables with no row version,
/// so without an in-transaction premise test two operators editing one
/// unreferenced value under the same tag both commit: one edit is lost and both
/// callers see `200`. The governed arm cannot reach that state because it
/// re-reads the value and re-derives the pin; this arm has no unit, so it
/// compares against the `held` the patch was authored over.
///
/// Staged the way the `replace` case is — by moving the store between the
/// caller's read and the call, which is what a concurrent commit does without
/// needing two live transactions.
#[tokio::test]
async fn the_direct_value_commit_refuses_a_value_the_store_has_moved_past() {
    let (repo, scope, provider) = harness().await;
    repo.declare_value(
        &scope,
        TENANT,
        TaxonomyClass::Brand,
        entry("acme", TaxonomyState::Active),
        stamp(),
    )
    .await
    .expect("declare");

    // What our caller read, and the edit they authored over it.
    let held = repo
        .find_value(&scope, TENANT, TaxonomyClass::Brand, &value("acme"))
        .await
        .expect("read")
        .expect("declared");
    let change = TaxonomyValueChange {
        proposal: TaxonomyValueProposal {
            class: TaxonomyClass::Brand,
            value: value("acme"),
            patch: TaxonomyValuePatch {
                display_name: Some("ACME Ltd".to_owned()),
                ..TaxonomyValuePatch::default()
            },
        },
        held: held.clone(),
    };

    // Somebody else relabels it first. This is the concurrent commit.
    let conn = provider.conn().expect("conn");
    let moved = TaxonomyEntry {
        display_name: "Acme Corporation".to_owned(),
        ..held.clone()
    };
    write_value_patch(
        &conn,
        &scope,
        TENANT,
        TaxonomyClass::Brand,
        &held,
        &moved,
        None,
        stamp(),
    )
    .await
    .expect("the concurrent commit lands");

    let refused =
        ApprovalService::commit_taxonomy_value_direct_in(&conn, &scope, TENANT, &change, stamp())
            .await
            .expect_err("a premise that moved must refuse");
    assert!(
        matches!(refused, DomainError::StaleVersion(_)),
        "the store's own refusal, typed 409 STALE_VERSION: {refused:?}"
    );

    let standing = repo
        .find_value(&scope, TENANT, TaxonomyClass::Brand, &value("acme"))
        .await
        .expect("read")
        .expect("declared");
    assert_eq!(
        standing.display_name, "Acme Corporation",
        "the refused edit wrote nothing over the concurrent one"
    );
}

/// `replace` refuses a tag the store has moved past — **inside its own
/// transaction**, not at the transport.
///
/// This is the property the handler-side comparison could not provide. That check
/// reads on a plain connection and the write re-reads in a transaction, so two
/// `PUT`s whose reads both precede either commit both pass it: A commits
/// `[alpha, acme]`, B's transaction then finds `acme` absent from its body and
/// **retires it**, and both callers get 200. The retire guard cannot close that
/// window — a value a concurrent author just added has no published references by
/// construction, so it is exactly the case the guard says nothing about.
///
/// Driven here by moving the store between the caller's read and the call, which
/// is what a concurrent commit does, without needing two live transactions.
#[tokio::test]
async fn replace_refuses_a_tag_the_store_has_moved_past() {
    let (repo, scope, _provider) = harness().await;
    repo.replace(
        &scope,
        TENANT,
        TaxonomyClass::Brand,
        vec![entry("alpha", TaxonomyState::Active)],
        &tag_of(TaxonomyClass::Brand, &[]),
        stamp(),
    )
    .await
    .expect("seed");

    // The tag our caller is holding, read before the world moved.
    let held = repo
        .list(&scope, TENANT, TaxonomyClass::Brand)
        .await
        .expect("read");
    let stale = tag_of(TaxonomyClass::Brand, &held);

    // Somebody else lands a value. This is A's commit in the scenario above.
    repo.replace(
        &scope,
        TENANT,
        TaxonomyClass::Brand,
        vec![
            entry("alpha", TaxonomyState::Active),
            entry("acme", TaxonomyState::Active),
        ],
        &stale,
        stamp(),
    )
    .await
    .expect("the other author lands");

    // Our caller now writes under the tag they read first. Under the old
    // arrangement this landed and retired `acme`.
    let result = repo
        .replace(
            &scope,
            TENANT,
            TaxonomyClass::Brand,
            vec![
                entry("alpha", TaxonomyState::Active),
                entry("zenith", TaxonomyState::Active),
            ],
            &stale,
            stamp(),
        )
        .await
        .expect("refused, not errored");

    assert!(
        result.stale,
        "the asserted tag no longer describes the store"
    );
    assert_eq!(
        values(&result.entries),
        [
            ("acme", TaxonomyState::Active),
            ("alpha", TaxonomyState::Active)
        ],
        "and nothing was written: the other author's value survives, ours is absent"
    );
}

/// The matching tag is accepted, so the guard is not simply refusing everything.
#[tokio::test]
async fn replace_accepts_the_tag_that_describes_the_store() {
    let (repo, scope, _provider) = harness().await;
    let held = repo
        .list(&scope, TENANT, TaxonomyClass::Brand)
        .await
        .expect("read");

    let result = repo
        .replace(
            &scope,
            TENANT,
            TaxonomyClass::Brand,
            vec![entry("alpha", TaxonomyState::Active)],
            &tag_of(TaxonomyClass::Brand, &held),
            stamp(),
        )
        .await
        .expect("accepted");

    assert!(!result.stale);
    assert_eq!(values(&result.entries), [("alpha", TaxonomyState::Active)]);
}

// ---------------------------------------------------------------------------
// `inst-tx-mutation` — the guard, against real referencing rows.
// ---------------------------------------------------------------------------

/// Seed one published overlay scoped to `(class, value)`.
///
/// Written through the entity rather than through `OverlayRepo`, for the reason
/// `sqlite_overlay_repo` gives in the mirror image of this situation: what is
/// under test is *this* repository's guard, and routing the fixture through the
/// overlay authoring path would make a failure here ambiguous between the two.
async fn publish_overlay_scoped(
    provider: &DBProvider<DbError>,
    id: u128,
    class: TaxonomyClass,
    value: &str,
    lifecycle: &str,
) {
    let conn = provider.conn().expect("conn");
    let row = price_overlay::ActiveModel {
        price_overlay_id: Set(Uuid::from_u128(id)),
        revision: Set(1),
        tenant_id: Set(TENANT),
        lifecycle_state: Set(lifecycle.to_owned()),
        scope_class: Set(class.scope_class().as_str().to_owned()),
        scope_value: Set(value.to_owned()),
        precedence: Set(10),
        effective_from: Set(None),
        effective_to: Set(None),
        tax_basis: Set("delegated_tariffs".to_owned()),
        disclosure: Set("restricted".to_owned()),
        target_ref: Set(serde_json::json!({"plans": []})),
        row_version: Set(0),
    };
    price_overlay::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed a price overlay");
}

/// A published overlay scope blocks its value's retirement — in every class.
///
/// D-120's widening. Before it, *"a region value retired cleanly while
/// region-scoped overlays still named it"*.
#[tokio::test]
async fn a_published_overlay_scope_blocks_the_retirement_in_every_class() {
    for class in TaxonomyClass::ALL {
        let (repo, scope, provider) = harness().await;
        replace_now(
            &repo,
            &scope,
            *class,
            vec![entry("in-use", TaxonomyState::Active)],
        )
        .await
        .expect("seed");
        publish_overlay_scoped(&provider, 0x0e_1a, *class, "in-use", "published").await;

        let result = replace_now(&repo, &scope, *class, vec![])
            .await
            .expect("the call succeeds; the retirement is refused");

        assert_eq!(
            result
                .report
                .violations
                .iter()
                .map(|v| v.code.as_str())
                .collect::<Vec<_>>(),
            [TAXONOMY_VALUE_IN_USE],
            "{class}: a published overlay scope is a reference (D-120)"
        );
        assert_eq!(
            values(&result.entries),
            if *class == TaxonomyClass::Region {
                vec![
                    (SEEDED_REGION, TaxonomyState::Retired),
                    ("in-use", TaxonomyState::Active),
                ]
            } else {
                vec![("in-use", TaxonomyState::Active)]
            },
            "{class}: a refused PUT changes nothing at all"
        );
    }
}

/// A **draft** overlay does not block: a value hostage to somebody's unpublished
/// experiment is a value no operator can retire on a schedule.
#[tokio::test]
async fn a_draft_overlay_scope_does_not_block_the_retirement() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Brand,
        vec![entry("acme", TaxonomyState::Active)],
    )
    .await
    .expect("seed");
    publish_overlay_scoped(&provider, 0x0e_1b, TaxonomyClass::Brand, "acme", "draft").await;

    let result = replace_now(&repo, &scope, TaxonomyClass::Brand, vec![])
        .await
        .expect("put");

    assert!(
        result.report.is_publishable(),
        "a draft is not a commitment"
    );
    assert_eq!(values(&result.entries), [("acme", TaxonomyState::Retired)]);
}

/// The counts are per class **and** per value: an overlay on `brand/acme` does
/// not block `partner/acme`.
///
/// Without the class predicate the guard would be a string match across four
/// universes, and one tenant's `gold` org tier would pin another taxonomy's
/// identically-named value forever.
#[tokio::test]
async fn the_reference_count_is_scoped_to_its_own_class() {
    let (_repo, _scope, provider) = harness().await;
    publish_overlay_scoped(
        &provider,
        0x0e_1a,
        TaxonomyClass::Brand,
        "acme",
        "published",
    )
    .await;

    let same = references_to(
        &provider.conn().expect("conn"),
        TENANT,
        TaxonomyClass::Brand,
        &ScopeValue::new("acme").expect("non-blank"),
    )
    .await
    .expect("count");
    let other = references_to(
        &provider.conn().expect("conn"),
        TENANT,
        TaxonomyClass::Partner,
        &ScopeValue::new("acme").expect("non-blank"),
    )
    .await
    .expect("count");

    assert_eq!(same.active_overlay_scopes, 1);
    assert_eq!(
        other.active_overlay_scopes, 0,
        "the same string in another class is another value"
    );
}

/// Seed one **published** price row on `region`.
async fn publish_price_row_in(provider: &DBProvider<DbError>, region: &str) {
    let conn = provider.conn().expect("conn");
    let plan_id = Uuid::from_u128(0x91a4);
    let plan_row = plan::ActiveModel {
        plan_id: Set(plan_id),
        revision: Set(1),
        tenant_id: Set(TENANT),
        lifecycle_state: Set("published".to_owned()),
        created_by: Set(Uuid::from_u128(0x4444)),
        created_at_utc: Set(now()),
        ..Default::default()
    };
    plan::Entity::insert(plan_row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &plan_row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the plan revision");

    let price_row = price::ActiveModel {
        price_id: Set(Uuid::from_u128(0xb001)),
        tenant_id: Set(TENANT),
        plan_id: Set(plan_id),
        currency: Set("EUR".to_owned()),
        region: Set(region.to_owned()),
        price_overlay: Set("base".to_owned()),
        phase: Set(Uuid::from_u128(0xf1)),
        price_eligibility: Set("all_subscriptions".to_owned()),
        charge_kind: Set("recurring".to_owned()),
        cohort: Set("none".to_owned()),
        dimension_key: Set(String::new()),
        tax_inclusive: Set(false),
        lifecycle_state: Set("published".to_owned()),
        created_by: Set(Uuid::from_u128(0x4444)),
        created_at_utc: Set(now()),
        row_version: Set(0),
        ..Default::default()
    };
    price::Entity::insert(price_row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &price_row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the price row");
}

/// Only `region` is counted on the price-row plane, because only `region` is an
/// axis of a price row (§3 step 2).
///
/// **The asymmetry is what is asserted, not a pair of zeros.** An earlier version
/// of this case seeded nothing and asserted `published_price_rows == 0` for the
/// three non-region classes — which passes whether the class predicate is there
/// or not, because there were no rows to count either way. It could not tell "not
/// counted because this class is not a row axis" from "not counted because the
/// table is empty", and a probe deleting the branch left it green.
#[tokio::test]
async fn the_row_plane_is_counted_for_region_alone() {
    let (_repo, _scope, provider) = harness().await;
    publish_price_row_in(&provider, "eu").await;
    let conn = provider.conn().expect("conn");
    let value = ScopeValue::new("eu").expect("non-blank");

    let region_refs = references_to(&conn, TENANT, TaxonomyClass::Region, &value)
        .await
        .expect("count");
    assert_eq!(
        region_refs.published_price_rows, 1,
        "a published row on `eu` is a reference on the region taxonomy"
    );

    for class in TaxonomyClass::ALL
        .iter()
        .filter(|c| **c != TaxonomyClass::Region)
    {
        let refs = references_to(&conn, TENANT, *class, &value)
            .await
            .expect("count");
        assert_eq!(
            refs.published_price_rows, 0,
            "{class} is not a price-row field, so the identically-named value is not a row \
             reference for it"
        );
    }
}

/// Dropping a region's `taxCategory` is refused while a published row resolves
/// through it — **without retiring anything** (D-245).
///
/// The body re-asserts the value as `active` and changes exactly one thing: the
/// marker. That is the point of the case. The retire guard beside it never fires
/// here, so a guard folded into `check_retirable` would have let this through,
/// and the failure would have surfaced months later as a plan that cannot
/// re-publish.
#[tokio::test]
async fn dropping_a_region_tax_category_a_published_row_leans_on_is_refused() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![region_entry("eu", Some("standard"), true)],
    )
    .await
    .expect("seed");
    // The row states no category of its own, so it resolves through the default.
    publish_price_row_in(&provider, "eu").await;

    let result = replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![region_entry("eu", None, true)],
    )
    .await
    .expect("refused, not errored");

    assert_eq!(
        result
            .report
            .violations
            .iter()
            .map(|v| v.code.as_str())
            .collect::<Vec<_>>(),
        [TAXONOMY_VALUE_IN_USE],
        "the marker removal is refused on its own, with nothing retired"
    );
    assert_eq!(
        values(&result.entries),
        [
            ("eu", TaxonomyState::Active),
            (SEEDED_REGION, TaxonomyState::Retired)
        ],
        "one transaction, one verdict: the taxonomy is unchanged"
    );
}

/// The same edit is accepted when nothing leans on the marker.
///
/// The control, and it is what separates "guards the dependency" from "refuses
/// every marker removal": same `PUT`, same shape, no published row.
#[tokio::test]
async fn dropping_a_region_tax_category_nothing_leans_on_is_accepted() {
    let (repo, scope, _provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![region_entry("eu", Some("standard"), true)],
    )
    .await
    .expect("seed");

    let result = replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![region_entry("eu", None, true)],
    )
    .await
    .expect("accepted");

    assert!(
        result.report.is_publishable(),
        "no row resolves through it, so nothing is guarded"
    );
}

/// A published price row blocks its region's retirement end to end.
#[tokio::test]
async fn a_published_price_row_blocks_its_regions_retirement() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![region_entry("eu", Some("standard"), true)],
    )
    .await
    .expect("seed");
    publish_price_row_in(&provider, "eu").await;

    let result = replace_now(&repo, &scope, TaxonomyClass::Region, vec![])
        .await
        .expect("refused, not errored");

    assert_eq!(
        result
            .report
            .violations
            .iter()
            .map(|v| v.code.as_str())
            .collect::<Vec<_>>(),
        [TAXONOMY_VALUE_IN_USE]
    );
    assert_eq!(
        values(&result.entries),
        [
            ("eu", TaxonomyState::Active),
            (SEEDED_REGION, TaxonomyState::Retired)
        ]
    );
}

// ---------------------------------------------------------------------------
// C4's readiness port.
// ---------------------------------------------------------------------------

/// An **undeclared** region and a declared one with no default category are
/// different facts, and the port keeps them apart.
///
/// C4 fails closed on the first — *"an unknown region fails closed"* — while the
/// second is exactly what `inst-td-policy`'s coalesce is about: a row carrying
/// its own `tax_category_ref` satisfies the check in a region whose taxonomy
/// carries no default. Collapsing the two would make that case unreachable.
#[tokio::test]
async fn readiness_distinguishes_an_unknown_region_from_one_with_no_category() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![region_entry("eu", None, true)],
    )
    .await
    .expect("seed");

    let declared = region_readiness(
        &provider.conn().expect("conn"),
        &scope,
        TENANT,
        &Region::new("eu").expect("ok"),
    )
    .await
    .expect("read");
    let unknown = region_readiness(
        &provider.conn().expect("conn"),
        &scope,
        TENANT,
        &Region::new("mars").expect("ok"),
    )
    .await
    .expect("read");

    assert_eq!(
        declared,
        Some(RegionTaxMarkers {
            tax_category: None,
            tax_rate_present: true
        }),
        "a declared region with no default category is Some(..) with None inside"
    );
    assert_eq!(
        unknown, None,
        "an undeclared region is None, and C4 fails closed on it"
    );
}

/// The two markers round-trip through the `PUT`, **both values of each**.
///
/// Every read-back of `tax_rate_present` in this suite asserted `true` until
/// 2026-08-20, and the one seed carrying `false` was written to be retired and
/// never read: no test anywhere in the crate round-tripped a `false` marker
/// through `TaxonomyRepo::replace`. It is the fail-closed half of C4's readiness
/// (`RegionTaxReadiness`), so a write path that hard-coded `Set(true)` in either
/// `insert_entry` or `update_entry` (`taxonomy_repo.rs`) left the suite entirely
/// green while making every region with no declared rate read as tax-ready — the
/// fail-open direction, and the one nobody notices until an invoice.
///
/// Both regions carry a category, so the marker is the only axis that differs.
#[tokio::test]
async fn the_region_markers_round_trip_through_the_put() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![
            region_entry("eu", Some("standard"), true),
            region_entry("us", Some("standard"), false),
        ],
    )
    .await
    .expect("seed");

    let readiness = region_readiness(
        &provider.conn().expect("conn"),
        &scope,
        TENANT,
        &Region::new("eu").expect("ok"),
    )
    .await
    .expect("read")
    .expect("declared");

    assert_eq!(readiness.tax_category.as_deref(), Some("standard"));
    assert!(readiness.tax_rate_present);

    let no_rate = region_readiness(
        &provider.conn().expect("conn"),
        &scope,
        TENANT,
        &Region::new("us").expect("ok"),
    )
    .await
    .expect("read")
    .expect("declared");
    assert_eq!(no_rate.tax_category.as_deref(), Some("standard"));
    assert!(
        !no_rate.tax_rate_present,
        "a declared region with no rate reads as not tax-ready: this marker is C4's \
         fail-closed half and a writer that forced it true fails open"
    );
}

/// A **retired** region declares nothing, so it has no readiness to report.
#[tokio::test]
async fn a_retired_region_has_no_readiness_and_is_not_in_the_active_universe() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![region_entry("eu", Some("standard"), true)],
    )
    .await
    .expect("seed");
    replace_now(&repo, &scope, TaxonomyClass::Region, vec![])
        .await
        .expect("retire it");

    let readiness = region_readiness(
        &provider.conn().expect("conn"),
        &scope,
        TENANT,
        &Region::new("eu").expect("ok"),
    )
    .await
    .expect("read");
    let universe = active_regions(&provider.conn().expect("conn"), &scope, TENANT)
        .await
        .expect("read");

    assert_eq!(readiness, None, "a retired value validates nothing");
    assert!(
        universe.is_empty(),
        "and is not in `inst-tx-region`'s universe"
    );
}

/// `active_regions` is the rule's universe and carries active values only.
#[tokio::test]
async fn the_active_region_universe_excludes_retirements() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![
            region_entry("eu", Some("standard"), true),
            region_entry("us", None, false),
        ],
    )
    .await
    .expect("seed");
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Region,
        vec![region_entry("eu", Some("standard"), true)],
    )
    .await
    .expect("retire us");

    let universe = active_regions(&provider.conn().expect("conn"), &scope, TENANT)
        .await
        .expect("read");

    assert_eq!(
        universe.iter().map(Region::as_str).collect::<Vec<_>>(),
        ["eu"]
    );
}

// ---------------------------------------------------------------------------
// The audit half of `inst-tx-mutation`.
// ---------------------------------------------------------------------------

/// *"Taxonomy mutation is tenant-admin config, audited"* — one record per `PUT`.
#[tokio::test]
async fn a_put_writes_exactly_one_audit_record_naming_the_taxonomy() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Brand,
        vec![
            entry("acme", TaxonomyState::Active),
            entry("zenith", TaxonomyState::Active),
        ],
    )
    .await
    .expect("put");

    let count = audit_records_for(&provider, "taxonomy/brand").await;

    assert_eq!(count, 1, "one PUT is one act, however many values it moved");
}

/// A **refused** `PUT` writes no audit record, because nothing happened.
#[tokio::test]
async fn a_refused_put_writes_no_audit_record() {
    let (repo, scope, provider) = harness().await;
    replace_now(
        &repo,
        &scope,
        TaxonomyClass::Brand,
        vec![entry("in-use", TaxonomyState::Active)],
    )
    .await
    .expect("seed");
    publish_overlay_scoped(
        &provider,
        0x0e_1a,
        TaxonomyClass::Brand,
        "in-use",
        "published",
    )
    .await;

    replace_now(&repo, &scope, TaxonomyClass::Brand, vec![])
        .await
        .expect("refused, not errored");

    let count = audit_records_for(&provider, "taxonomy/brand").await;

    assert_eq!(
        count, 1,
        "the seeding PUT only - the refused one wrote nothing"
    );
}

// ---------------------------------------------------------------------------
// The customer-group taxonomy (`inst-cg-taxonomy`) — its own table, its own
// route, and deliberately no `TaxonomyClass` arm. See
// `taxonomy_repo::list_customer_groups`'s doc for why this is a parallel
// method rather than a fifth match arm above.
// ---------------------------------------------------------------------------

/// `replace_customer_groups` under the tag the store currently renders —
/// `replace_now`'s sibling.
async fn replace_customer_groups_now(
    repo: &TaxonomyRepo,
    scope: &AccessScope,
    entries: Vec<TaxonomyEntry>,
) -> Result<Replaced, bss_pricing::infra::storage::RepoError> {
    let held = repo
        .list_customer_groups(scope, TENANT)
        .await
        .expect("read back");
    repo.replace_customer_groups(
        scope,
        TENANT,
        entries,
        &customer_group_tag_of(&held),
        stamp(),
    )
    .await
}

/// Seed one published overlay scoped to `(customerGroup, value)` —
/// `publish_overlay_scoped`'s sibling, written directly through the entity for
/// the same reason: what is under test is the guard, not the overlay
/// lifecycle.
async fn publish_customer_group_overlay_scoped(
    provider: &DBProvider<DbError>,
    id: u128,
    value: &str,
    lifecycle: &str,
) {
    let conn = provider.conn().expect("conn");
    let row = price_overlay::ActiveModel {
        price_overlay_id: Set(Uuid::from_u128(id)),
        revision: Set(1),
        tenant_id: Set(TENANT),
        lifecycle_state: Set(lifecycle.to_owned()),
        scope_class: Set(ScopeClass::CustomerGroup.as_str().to_owned()),
        scope_value: Set(value.to_owned()),
        precedence: Set(10),
        effective_from: Set(None),
        effective_to: Set(None),
        tax_basis: Set("delegated_tariffs".to_owned()),
        disclosure: Set("restricted".to_owned()),
        target_ref: Set(serde_json::json!({"plans": []})),
        row_version: Set(0),
    };
    price_overlay::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed a price overlay");
}

/// The round trip: a first `PUT` declares the whole customer-group set.
#[tokio::test]
async fn a_first_put_declares_the_whole_customer_group_set() {
    let (repo, scope, _provider) = harness().await;

    let result = replace_customer_groups_now(
        &repo,
        &scope,
        vec![
            entry("gold", TaxonomyState::Active),
            entry("silver", TaxonomyState::Active),
        ],
    )
    .await
    .expect("the first put lands");

    assert!(result.report.is_publishable(), "nothing to guard yet");
    assert_eq!(
        values(&result.entries),
        [
            ("gold", TaxonomyState::Active),
            ("silver", TaxonomyState::Active)
        ]
    );
}

/// A value the body omits is retired, not deleted — the same whole-set
/// semantics as the four-class taxonomy.
#[tokio::test]
async fn a_customer_group_value_the_body_omits_is_retired_and_still_readable() {
    let (repo, scope, _provider) = harness().await;
    replace_customer_groups_now(
        &repo,
        &scope,
        vec![
            entry("gold", TaxonomyState::Active),
            entry("silver", TaxonomyState::Active),
        ],
    )
    .await
    .expect("seed");

    let result =
        replace_customer_groups_now(&repo, &scope, vec![entry("gold", TaxonomyState::Active)])
            .await
            .expect("the second put lands");

    assert_eq!(
        values(&result.entries),
        [
            ("gold", TaxonomyState::Active),
            ("silver", TaxonomyState::Retired)
        ]
    );
}

/// **The retire guard's one plane.** A published `customerGroup`-scoped
/// overlay blocks the value's retirement — the claim `inst-cg-taxonomy` makes
/// and the point of building this table at all.
#[tokio::test]
async fn a_published_overlay_scope_blocks_a_customer_groups_retirement() {
    let (repo, scope, provider) = harness().await;
    replace_customer_groups_now(&repo, &scope, vec![entry("in-use", TaxonomyState::Active)])
        .await
        .expect("seed");
    publish_customer_group_overlay_scoped(&provider, 0x0c_1a, "in-use", "published").await;

    let result = replace_customer_groups_now(&repo, &scope, vec![])
        .await
        .expect("the call succeeds; the retirement is refused");

    assert_eq!(
        result
            .report
            .violations
            .iter()
            .map(|v| v.code.as_str())
            .collect::<Vec<_>>(),
        [TAXONOMY_VALUE_IN_USE],
        "a published overlay scope is a reference"
    );
    assert_eq!(
        values(&result.entries),
        [("in-use", TaxonomyState::Active)],
        "a refused PUT changes nothing at all"
    );
}

/// A **draft** overlay does not block — the four-class guard's own reading,
/// carried unchanged: a value hostage to somebody's unpublished experiment is a
/// value no operator can retire on a schedule.
#[tokio::test]
async fn a_draft_overlay_scope_does_not_block_a_customer_groups_retirement() {
    let (repo, scope, provider) = harness().await;
    replace_customer_groups_now(&repo, &scope, vec![entry("gold", TaxonomyState::Active)])
        .await
        .expect("seed");
    publish_customer_group_overlay_scoped(&provider, 0x0c_1b, "gold", "draft").await;

    let result = replace_customer_groups_now(&repo, &scope, vec![])
        .await
        .expect("put");

    assert!(
        result.report.is_publishable(),
        "a draft is not a commitment"
    );
    assert_eq!(values(&result.entries), [("gold", TaxonomyState::Retired)]);
}

/// The reference count is scoped to `customerGroup` alone: an overlay on
/// `brand/gold` must not block `customerGroup/gold` — the same identically-named
/// value, two different universes.
#[tokio::test]
async fn a_customer_groups_reference_count_is_not_shared_with_another_class() {
    let (repo, scope, provider) = harness().await;
    replace_customer_groups_now(&repo, &scope, vec![entry("gold", TaxonomyState::Active)])
        .await
        .expect("seed");
    publish_overlay_scoped(
        &provider,
        0x0c_1c,
        TaxonomyClass::Brand,
        "gold",
        "published",
    )
    .await;

    let result = replace_customer_groups_now(&repo, &scope, vec![])
        .await
        .expect("put");

    assert!(
        result.report.is_publishable(),
        "a brand-scoped overlay on the identically-named value must not block a customer-group \
         retirement"
    );
    assert_eq!(values(&result.entries), [("gold", TaxonomyState::Retired)]);
}

// ---------------------------------------------------------------------------
// The membership plane — Task 8's review finding against Task 6's cut of this
// guard: `references_to_customer_group` counted published overlay scopes and
// nothing else, so a group with live members retired unguarded and those
// memberships were left pointing at a retired value with nothing having
// checked.
// ---------------------------------------------------------------------------

/// Seed one `pricing_group_membership` row directly through the entity —
/// `publish_customer_group_overlay_scoped`'s sibling, one table over: what is
/// under test is the guard, not `group_membership_repo::enroll`'s own overlap
/// invariant, which this seed does not need to satisfy.
async fn seed_customer_group_membership(
    provider: &DBProvider<DbError>,
    membership_id: u128,
    payer_tenant_id: u128,
    group_value: &str,
    effective_from: OffsetDateTime,
    effective_to: Option<OffsetDateTime>,
) {
    use bss_pricing::infra::storage::entity::group_membership;

    let conn = provider.conn().expect("conn");
    let row = group_membership::ActiveModel {
        membership_id: Set(Uuid::from_u128(membership_id)),
        tenant_id: Set(TENANT),
        payer_tenant_id: Set(Uuid::from_u128(payer_tenant_id)),
        group_value: Set(group_value.to_owned()),
        effective_from: Set(effective_from),
        effective_to: Set(effective_to),
        created_by: Set(Uuid::from_u128(0xac_10)),
        created_at_utc: Set(effective_from),
        row_version: Set(0),
    };
    group_membership::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed a group membership");
}

/// **The gap this task closes.** A live membership — open-ended, started well
/// before `now()` — blocks the group's retirement, with **no overlay
/// anywhere**: the seed below writes only a `pricing_group_membership` row.
///
/// To redden this: read `references_to_customer_group` as Task 6 left it —
/// it queries `price_overlay` alone and never touches
/// `pricing_group_membership`. Against a seed carrying no overlay at all, that
/// version counts zero references either way and the retirement sails
/// through; this is the assertion that catches it, and it fails today for
/// exactly that reason.
#[tokio::test]
async fn a_live_membership_blocks_a_customer_groups_retirement() {
    let (repo, scope, provider) = harness().await;
    replace_customer_groups_now(&repo, &scope, vec![entry("gold", TaxonomyState::Active)])
        .await
        .expect("seed");
    seed_customer_group_membership(
        &provider,
        0x0c_2a,
        0x0c_2b,
        "gold",
        now() - time::Duration::days(30),
        None,
    )
    .await;

    let result = replace_customer_groups_now(&repo, &scope, vec![])
        .await
        .expect("the call succeeds; the retirement is refused");

    assert_eq!(
        result
            .report
            .violations
            .iter()
            .map(|v| v.code.as_str())
            .collect::<Vec<_>>(),
        [TAXONOMY_VALUE_IN_USE],
        "a live membership is a reference, with no overlay in sight"
    );
    assert!(
        result.report.violations[0].detail.contains("membership"),
        "the refusal must name the membership plane it found, not just the shared code: {}",
        result.report.violations[0].detail
    );
    assert_eq!(
        values(&result.entries),
        [("gold", TaxonomyState::Active)],
        "a refused PUT changes nothing at all"
    );
}

/// **The decision on a membership that has not started yet.** `now()` sits
/// before `effective_from` — the group is a commitment already recorded, not
/// a draft still open to redirection — and it blocks the retirement exactly
/// as an already-active one does.
#[tokio::test]
async fn a_membership_that_has_not_started_yet_blocks_a_customer_groups_retirement() {
    let (repo, scope, provider) = harness().await;
    replace_customer_groups_now(&repo, &scope, vec![entry("gold", TaxonomyState::Active)])
        .await
        .expect("seed");
    seed_customer_group_membership(
        &provider,
        0x0c_2c,
        0x0c_2d,
        "gold",
        now() + time::Duration::days(30),
        None,
    )
    .await;

    let result = replace_customer_groups_now(&repo, &scope, vec![])
        .await
        .expect("the call succeeds; the retirement is refused");

    assert_eq!(
        result
            .report
            .violations
            .iter()
            .map(|v| v.code.as_str())
            .collect::<Vec<_>>(),
        [TAXONOMY_VALUE_IN_USE],
        "a scheduled membership is still a live reference"
    );
}

/// **The other half of the decision.** A membership whose interval already
/// ended before `now()` is not a live reference — the same "history is not a
/// reference" reading the module doc gives a `superseded` price row.
#[tokio::test]
async fn an_ended_membership_does_not_block_a_customer_groups_retirement() {
    let (repo, scope, provider) = harness().await;
    replace_customer_groups_now(&repo, &scope, vec![entry("gold", TaxonomyState::Active)])
        .await
        .expect("seed");
    seed_customer_group_membership(
        &provider,
        0x0c_2e,
        0x0c_2f,
        "gold",
        now() - time::Duration::days(60),
        Some(now() - time::Duration::days(1)),
    )
    .await;

    let result = replace_customer_groups_now(&repo, &scope, vec![])
        .await
        .expect("put");

    assert!(
        result.report.is_publishable(),
        "an ended membership is history, not a reference"
    );
    assert_eq!(values(&result.entries), [("gold", TaxonomyState::Retired)]);
}

/// A foreign tenant's customer-group taxonomy is invisible — the same SQL-level
/// BOLA guard as the four-class store.
#[tokio::test]
async fn a_foreign_tenants_customer_group_taxonomy_is_invisible() {
    let (repo, scope, _provider) = harness().await;
    replace_customer_groups_now(&repo, &scope, vec![entry("gold", TaxonomyState::Active)])
        .await
        .expect("seed");

    let by_argument = repo
        .list_customer_groups(&AccessScope::allow_all(), OTHER_TENANT)
        .await
        .expect("read");
    assert!(
        by_argument.is_empty(),
        "another tenant's values are not visible to the argument"
    );

    // Crossed, for the sibling case's reason: the scope is the intruder's and the
    // argument is the owner's, so only the compiled scope can refuse.
    let by_scope = repo
        .list_customer_groups(&AccessScope::for_tenant(OTHER_TENANT), TENANT)
        .await
        .expect("read");
    assert!(
        by_scope.is_empty(),
        "nor to a caller scoped elsewhere reaching for the owner's rows by name"
    );

    assert_eq!(
        repo.list_customer_groups(&AccessScope::for_tenant(TENANT), TENANT)
            .await
            .expect("read")
            .len(),
        1,
        "and the owner, scoped to itself, still sees its own value"
    );
}

/// `replace_customer_groups` refuses a tag the store has moved past, inside its
/// own transaction — `replace_refuses_a_tag_the_store_has_moved_past`'s claim,
/// carried to the one-table store.
#[tokio::test]
async fn replace_customer_groups_refuses_a_tag_the_store_has_moved_past() {
    let (repo, scope, _provider) = harness().await;
    repo.replace_customer_groups(
        &scope,
        TENANT,
        vec![entry("gold", TaxonomyState::Active)],
        &customer_group_tag_of(&[]),
        stamp(),
    )
    .await
    .expect("seed");

    let held = repo
        .list_customer_groups(&scope, TENANT)
        .await
        .expect("read");
    let stale = customer_group_tag_of(&held);

    repo.replace_customer_groups(
        &scope,
        TENANT,
        vec![
            entry("gold", TaxonomyState::Active),
            entry("silver", TaxonomyState::Active),
        ],
        &stale,
        stamp(),
    )
    .await
    .expect("the other author lands");

    let result = repo
        .replace_customer_groups(
            &scope,
            TENANT,
            vec![
                entry("gold", TaxonomyState::Active),
                entry("bronze", TaxonomyState::Active),
            ],
            &stale,
            stamp(),
        )
        .await
        .expect("refused, not errored");

    assert!(
        result.stale,
        "the asserted tag no longer describes the store"
    );
    assert_eq!(
        values(&result.entries),
        [
            ("gold", TaxonomyState::Active),
            ("silver", TaxonomyState::Active)
        ],
        "and nothing was written: the other author's value survives, ours is absent"
    );
}

/// One audit record per `PUT`, naming the customer-group taxonomy.
#[tokio::test]
async fn a_customer_group_put_writes_exactly_one_audit_record() {
    let (repo, scope, provider) = harness().await;
    replace_customer_groups_now(
        &repo,
        &scope,
        vec![
            entry("gold", TaxonomyState::Active),
            entry("silver", TaxonomyState::Active),
        ],
    )
    .await
    .expect("put");

    let count = audit_records_for(&provider, "taxonomy/customer_group").await;

    assert_eq!(count, 1, "one PUT is one act, however many values it moved");
}

// ---------------------------------------------------------------------------
// The rounding vocabulary is its own taxonomy, on both planes (D-334).
// ---------------------------------------------------------------------------

/// `replace_rounding_policies` under the tag the store currently renders —
/// [`replace_customer_groups_now`]'s shape on the second single-table taxonomy.
async fn replace_rounding_policies_now(
    repo: &TaxonomyRepo,
    scope: &AccessScope,
    entries: Vec<TaxonomyEntry>,
) -> Result<Replaced, bss_pricing::infra::storage::RepoError> {
    let held = repo
        .list_rounding_policies(scope, TENANT)
        .await
        .expect("read back");
    repo.replace_rounding_policies(
        scope,
        TENANT,
        entries,
        &rounding_policy_tag_of(&held),
        stamp(),
    )
    .await
}

/// **The third store's cross-tenant probe** — the one the other two now have.
///
/// The rounding vocabulary is what a price row resolves its rounding through, so a
/// read path that trusted its `tenant_id` argument over the caller's compiled
/// scope is a cross-tenant read of a value that decides money.
#[tokio::test]
async fn a_foreign_tenants_rounding_policy_taxonomy_is_invisible() {
    let (repo, scope, _provider) = harness().await;
    replace_rounding_policies_now(&repo, &scope, vec![entry("half_up", TaxonomyState::Active)])
        .await
        .expect("seed");

    let by_argument = repo
        .list_rounding_policies(&AccessScope::allow_all(), OTHER_TENANT)
        .await
        .expect("read");
    assert!(
        by_argument.is_empty(),
        "another tenant's values are not visible to the argument"
    );

    // Crossed, as the two sibling cases are: the scope is the intruder's and the
    // argument is the owner's, so only the compiled scope can refuse.
    let by_scope = repo
        .list_rounding_policies(&AccessScope::for_tenant(OTHER_TENANT), TENANT)
        .await
        .expect("read");
    assert!(
        by_scope.is_empty(),
        "nor to a caller scoped elsewhere reaching for the owner's rows by name"
    );

    assert_eq!(
        repo.list_rounding_policies(&AccessScope::for_tenant(TENANT), TENANT)
            .await
            .expect("read")
            .len(),
        1,
        "and the owner, scoped to itself, still sees its own value"
    );
}

/// A rounding-vocabulary `PUT` is audited **under the rounding taxonomy**, not
/// under the customer-group one.
///
/// `inst-tx-mutation` requires the mutation to be audited, and an audit record is
/// only a record of the act it names: the two vocabularies are governed by two
/// routes and retired against two different reference planes, so an auditor
/// filtering the per-tenant policy segment must be able to tell a rounding change
/// from a customer-group change. Both wrote `taxonomy/customer_group` until this
/// case existed, which made the D-334 vocabulary invisible *and* the
/// customer-group trail wrong in the same record.
#[tokio::test]
async fn a_rounding_policy_put_is_recorded_under_its_own_taxonomy() {
    let (repo, scope, provider) = harness().await;

    replace_rounding_policies_now(
        &repo,
        &scope,
        vec![
            entry("half_up", TaxonomyState::Active),
            entry("bankers", TaxonomyState::Active),
        ],
    )
    .await
    .expect("put");

    assert_eq!(
        audit_records_for(&provider, "taxonomy/rounding-policies").await,
        1,
        "one PUT is one act, recorded under the taxonomy it moved"
    );
    assert_eq!(
        audit_records_for(&provider, "taxonomy/customer_group").await,
        0,
        "and nothing is recorded against a taxonomy this call never touched"
    );
}

/// Seed one **published** price row whose rounding policy was resolved from the
/// tenant default at publish: the authored column is NULL and the frozen
/// resolution lives in `resolved_rounding_policy`.
///
/// That is what `publish_rows` writes since `pricing_price` — the split the
/// retirement guard's count has to follow. Written through the entity for
/// [`publish_price_row_in`]'s reason: what is under test is the guard, not the
/// publish path.
async fn publish_price_row_resolving(provider: &DBProvider<DbError>, resolved: &str) {
    let conn = provider.conn().expect("conn");
    let plan_id = Uuid::from_u128(0x91a5);
    let plan_row = plan::ActiveModel {
        plan_id: Set(plan_id),
        revision: Set(1),
        tenant_id: Set(TENANT),
        lifecycle_state: Set("published".to_owned()),
        created_by: Set(Uuid::from_u128(0x4444)),
        created_at_utc: Set(now()),
        ..Default::default()
    };
    plan::Entity::insert(plan_row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &plan_row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the plan revision");

    let price_row = price::ActiveModel {
        price_id: Set(Uuid::from_u128(0xb002)),
        tenant_id: Set(TENANT),
        plan_id: Set(plan_id),
        currency: Set("EUR".to_owned()),
        region: Set("eu".to_owned()),
        price_overlay: Set("base".to_owned()),
        phase: Set(Uuid::from_u128(0xf1)),
        price_eligibility: Set("all_subscriptions".to_owned()),
        charge_kind: Set("recurring".to_owned()),
        cohort: Set("none".to_owned()),
        dimension_key: Set(String::new()),
        tax_inclusive: Set(false),
        lifecycle_state: Set("published".to_owned()),
        // The authored column stays empty: this row leaned on the tenant default
        // and the publish deliberately did not write one over it.
        rounding_policy_ref: Set(None),
        resolved_rounding_policy: Set(Some(resolved.to_owned())),
        created_by: Set(Uuid::from_u128(0x4444)),
        created_at_utc: Set(now()),
        row_version: Set(0),
        ..Default::default()
    };
    price::Entity::insert(price_row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &price_row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the price row");
}

/// A value **only the frozen resolution names** still blocks its retirement.
///
/// The guard counted `rounding_policy_ref` alone, which was the whole story until
/// `pricing_price` split the resolution into its own column. After the split a
/// row that resolved the tenant default carries the value in
/// `resolved_rounding_policy` and NULL in the authored column — so once the
/// default has moved on, nothing counted it and the operator was allowed to retire
/// a value already-frozen published rows resolve against. That is exactly the
/// dangling reference `check_rounding_policy_retirable`'s own message exists to
/// prevent, and the tenant default deliberately names something else here so the
/// `default_names_it` half cannot be what refuses.
#[tokio::test]
async fn a_value_only_the_frozen_resolution_names_still_blocks_retirement() {
    let (repo, scope, provider) = harness().await;
    replace_rounding_policies_now(
        &repo,
        &scope,
        vec![
            entry("half_up", TaxonomyState::Active),
            entry("bankers", TaxonomyState::Active),
        ],
    )
    .await
    .expect("declare the vocabulary");
    publish_price_row_resolving(&provider, "half_up").await;

    let result =
        replace_rounding_policies_now(&repo, &scope, vec![entry("bankers", TaxonomyState::Active)])
            .await
            .expect("refused, not errored");

    assert!(
        !result.report.is_publishable(),
        "a published row resolves against `half_up`, so it cannot be retired"
    );
    assert_eq!(
        values(&result.entries),
        [
            ("bankers", TaxonomyState::Active),
            ("half_up", TaxonomyState::Active)
        ],
        "and a refused retirement writes nothing at all"
    );
}

// ---------------------------------------------------------------------------
// The GL-code vocabulary is its own taxonomy, on both planes (D-356).
// ---------------------------------------------------------------------------

/// `replace_gl_codes` under the tag the store currently renders —
/// [`replace_rounding_policies_now`]'s shape on the third single-table taxonomy.
async fn replace_gl_codes_now(
    repo: &TaxonomyRepo,
    scope: &AccessScope,
    entries: Vec<TaxonomyEntry>,
) -> Result<Replaced, bss_pricing::infra::storage::RepoError> {
    let held = repo.list_gl_codes(scope, TENANT).await.expect("read back");
    repo.replace_gl_codes(scope, TENANT, entries, &gl_code_tag_of(&held), stamp())
        .await
}

fn declared_gl_codes(values: &[&str]) -> std::collections::BTreeSet<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

/// A first write declares the set, an omitted value is **retired rather than
/// deleted**, and `active_gl_codes` — the publish rule's operand — reads the
/// `active` values alone.
///
/// The three facts are pinned in one case because they are one contract: what a
/// `PUT` writes is exactly what the publish rule will read, and a retirement
/// that stayed visible to the rule would let a retired code validate a new
/// descriptor set against itself.
#[tokio::test]
async fn a_gl_code_put_declares_retires_by_omission_and_the_active_read_follows() {
    let (repo, scope, provider) = harness().await;
    let conn = provider.conn().expect("conn");

    assert!(
        active_gl_codes(&conn, &scope, TENANT)
            .await
            .expect("read")
            .is_empty(),
        "a tenant that declared nothing reads an empty set - the opt-out state"
    );

    let declared = replace_gl_codes_now(
        &repo,
        &scope,
        vec![
            entry("4000-REV", TaxonomyState::Active),
            entry("4010-TAX", TaxonomyState::Active),
        ],
    )
    .await
    .expect("declare");
    assert!(!declared.stale && declared.report.is_publishable());
    assert_eq!(
        active_gl_codes(&conn, &scope, TENANT).await.expect("read"),
        declared_gl_codes(&["4000-REV", "4010-TAX"])
    );

    let retired = replace_gl_codes_now(
        &repo,
        &scope,
        vec![entry("4000-REV", TaxonomyState::Active)],
    )
    .await
    .expect("retire by omission");
    assert!(!retired.stale && retired.report.is_publishable());
    assert_eq!(
        values(&retired.entries),
        [
            ("4000-REV", TaxonomyState::Active),
            ("4010-TAX", TaxonomyState::Retired)
        ],
        "the omitted code is retired, never deleted"
    );
    assert_eq!(
        active_gl_codes(&conn, &scope, TENANT).await.expect("read"),
        declared_gl_codes(&["4000-REV"]),
        "and the publish rule's operand no longer contains it"
    );
}

/// The cross-tenant probe the other three stores carry.
///
/// A GL code is what an ERP posts a tenant's revenue against, so a read path
/// that trusted its `tenant_id` argument over the caller's compiled scope would
/// validate one tenant's catalog against another's chart of accounts.
#[tokio::test]
async fn a_foreign_tenants_gl_code_taxonomy_is_invisible() {
    let (repo, scope, provider) = harness().await;
    replace_gl_codes_now(
        &repo,
        &scope,
        vec![entry("4000-REV", TaxonomyState::Active)],
    )
    .await
    .expect("seed");

    let by_argument = repo
        .list_gl_codes(&AccessScope::allow_all(), OTHER_TENANT)
        .await
        .expect("read");
    assert!(
        by_argument.is_empty(),
        "another tenant's values are not visible to the argument"
    );

    let by_scope = repo
        .list_gl_codes(&AccessScope::for_tenant(OTHER_TENANT), TENANT)
        .await
        .expect("read");
    assert!(
        by_scope.is_empty(),
        "nor to a caller scoped elsewhere reaching for the owner's rows by name"
    );

    let conn = provider.conn().expect("conn");
    assert!(
        active_gl_codes(&conn, &AccessScope::for_tenant(OTHER_TENANT), TENANT)
            .await
            .expect("read")
            .is_empty(),
        "the publish rule's read is scoped the same way"
    );
    assert_eq!(
        repo.list_gl_codes(&AccessScope::for_tenant(TENANT), TENANT)
            .await
            .expect("read")
            .len(),
        1,
        "and the owner, scoped to itself, still sees its own value"
    );
}

/// A GL-code `PUT` is audited **under the GL-code taxonomy**, not under either
/// of its two siblings.
///
/// `inst-tx-mutation`'s reason as the rounding case gives it: three vocabularies
/// are governed by three routes and retired against three reference planes, so
/// an auditor filtering the per-tenant policy segment must be able to tell them
/// apart.
#[tokio::test]
async fn a_gl_code_put_is_recorded_under_its_own_taxonomy() {
    let (repo, scope, provider) = harness().await;

    replace_gl_codes_now(
        &repo,
        &scope,
        vec![
            entry("4000-REV", TaxonomyState::Active),
            entry("4010-TAX", TaxonomyState::Active),
        ],
    )
    .await
    .expect("put");

    assert_eq!(
        audit_records_for(&provider, "taxonomy/gl-codes").await,
        1,
        "one PUT is one act, recorded under the taxonomy it moved"
    );
    assert_eq!(
        audit_records_for(&provider, "taxonomy/rounding-policies").await
            + audit_records_for(&provider, "taxonomy/customer_group").await,
        0,
        "and nothing is recorded against a taxonomy this call never touched"
    );
}

/// Seed one plan revision whose billing descriptor set names `gl_code`, and
/// leave it in `state`.
///
/// Through the entities rather than the publish path, for
/// [`publish_price_row_resolving`]'s reason: what is under test is the guard.
/// The descriptor row is inserted **while the revision is a draft** — its
/// append-only trigger refuses an insert under a non-draft parent — and the
/// revision is then flipped, which is the same flip `publish_revision` performs.
async fn seed_revision_naming_gl_code(
    provider: &DBProvider<DbError>,
    plan_id: Uuid,
    gl_code: &str,
    state: &str,
) {
    let conn = provider.conn().expect("conn");
    let plan_row = plan::ActiveModel {
        plan_id: Set(plan_id),
        revision: Set(1),
        tenant_id: Set(TENANT),
        lifecycle_state: Set("draft".to_owned()),
        created_by: Set(Uuid::from_u128(0x4444)),
        created_at_utc: Set(now()),
        ..Default::default()
    };
    plan::Entity::insert(plan_row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &plan_row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the plan revision");

    let descriptors = plan_descriptor_set::ActiveModel {
        plan_id: Set(plan_id),
        plan_revision: Set(1),
        tenant_id: Set(TENANT),
        invoice_line_template: Set(Some("{plan}".to_owned())),
        gl_code: Set(Some(gl_code.to_owned())),
        itemization_rule: Set(Some("per_charge".to_owned())),
        additional_fields: Set(serde_json::json!({})),
    };
    plan_descriptor_set::Entity::insert(descriptors.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &descriptors)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the descriptor set");

    if state != "draft" {
        let moved = plan::Entity::update_many()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .col_expr(
                plan::Column::LifecycleState,
                sea_orm::sea_query::Expr::value(state),
            )
            .filter(
                Condition::all()
                    .add(plan::Column::PlanId.eq(plan_id))
                    .add(plan::Column::Revision.eq(1_i64)),
            )
            .exec(&conn)
            .await
            .expect("flip the revision");
        assert_eq!(moved.rows_affected, 1, "the seed must have moved one row");
    }
}

/// A code a **published** revision's descriptor set names cannot be retired, and
/// nothing is written; a code only a **draft** names can.
///
/// Both halves in one case, because either alone is satisfied by a wrong guard: a
/// guard that refused every retirement passes the first, and a guard that counted
/// nothing passes the second. Drafts are not counted for the rounding guard's
/// reason - a draft's author can still change the code, while a published
/// revision's is frozen into a `CatalogVersion` an ERP posts against.
#[tokio::test]
async fn a_gl_code_a_published_descriptor_set_names_cannot_be_retired_while_a_drafts_can() {
    let (repo, scope, provider) = harness().await;
    replace_gl_codes_now(
        &repo,
        &scope,
        vec![
            entry("4000-REV", TaxonomyState::Active),
            entry("4010-TAX", TaxonomyState::Active),
            entry("4020-WIP", TaxonomyState::Active),
        ],
    )
    .await
    .expect("declare the vocabulary");
    seed_revision_naming_gl_code(&provider, Uuid::from_u128(0x91b1), "4000-REV", "published").await;
    seed_revision_naming_gl_code(&provider, Uuid::from_u128(0x91b2), "4020-WIP", "draft").await;

    let refused = replace_gl_codes_now(
        &repo,
        &scope,
        vec![
            entry("4010-TAX", TaxonomyState::Active),
            entry("4020-WIP", TaxonomyState::Active),
        ],
    )
    .await
    .expect("refused, not errored");
    assert!(
        !refused.report.is_publishable(),
        "a published revision's descriptor set names `4000-REV`, so it cannot be retired"
    );
    assert_eq!(refused.report.violations[0].code, TAXONOMY_VALUE_IN_USE);
    assert!(
        refused.report.violations[0]
            .detail
            .contains("1 published plan revision(s)"),
        "the refusal counts the frozen revisions: {}",
        refused.report.violations[0].detail
    );
    assert_eq!(
        values(&refused.entries),
        [
            ("4000-REV", TaxonomyState::Active),
            ("4010-TAX", TaxonomyState::Active),
            ("4020-WIP", TaxonomyState::Active)
        ],
        "and a refused retirement writes nothing at all"
    );

    let allowed = replace_gl_codes_now(
        &repo,
        &scope,
        vec![
            entry("4000-REV", TaxonomyState::Active),
            entry("4010-TAX", TaxonomyState::Active),
        ],
    )
    .await
    .expect("retire the code only a draft names");
    assert!(
        allowed.report.is_publishable(),
        "a draft's author can still change the code, so a draft does not pin it"
    );
    assert_eq!(
        values(&allowed.entries),
        [
            ("4000-REV", TaxonomyState::Active),
            ("4010-TAX", TaxonomyState::Active),
            ("4020-WIP", TaxonomyState::Retired)
        ]
    );
}

// ---------------------------------------------------------------------------
// D-354: the seeded region.
// ---------------------------------------------------------------------------

/// A tenant holding no region row reads `{global: active}` from every region
/// reader, with the fail-closed markers — and nothing is written by reading.
#[tokio::test]
async fn a_tenant_with_no_region_row_reads_the_seeded_global_everywhere() {
    let (repo, scope, provider) = harness().await;
    let conn = provider.conn().expect("conn");

    let listed = repo
        .list(&scope, TENANT, TaxonomyClass::Region)
        .await
        .expect("list");
    assert_eq!(listed, vec![seeded_region()]);
    assert_eq!(
        values(&listed),
        vec![(SEEDED_REGION, TaxonomyState::Active)]
    );
    let universe = active_regions(&conn, &scope, TENANT)
        .await
        .expect("universe");
    assert_eq!(
        universe,
        std::iter::once(Region::new(SEEDED_REGION).expect("ok")).collect()
    );
    assert_eq!(
        region_readiness(
            &conn,
            &scope,
            TENANT,
            &Region::new(SEEDED_REGION).expect("ok")
        )
        .await
        .expect("read"),
        Some(RegionTaxMarkers::default()),
        "declared, with no tax fact asserted"
    );
    assert_eq!(
        region_readiness(&conn, &scope, TENANT, &Region::new("mars").expect("ok"))
            .await
            .expect("read"),
        None,
        "the seed does not make every region known"
    );
    let map = region_readiness_map(&conn, &scope, TENANT)
        .await
        .expect("map");
    assert_eq!(map.len(), 1);
    assert_eq!(map.get(SEEDED_REGION), Some(&RegionTaxMarkers::default()));

    // Reading wrote nothing: the brand list of the same tenant is untouched, and
    // a second read of the region list is the same virtual entry.
    assert!(
        repo.list(&scope, TENANT, TaxonomyClass::Brand)
            .await
            .expect("brand")
            .is_empty()
    );
    assert_eq!(
        repo.list(&scope, TENANT, TaxonomyClass::Region)
            .await
            .expect("list again"),
        vec![seeded_region()]
    );
}

/// The tenant's first region write materialises the seed before it lands, so
/// the value the reads answered is the row the write goes on to sit beside.
#[tokio::test]
async fn the_first_region_write_materialises_the_seed_beside_it() {
    let (repo, scope, provider) = harness().await;
    let conn = provider.conn().expect("conn");

    repo.declare_value(
        &scope,
        TENANT,
        TaxonomyClass::Region,
        region_entry("eu", Some("standard"), true),
        stamp(),
    )
    .await
    .expect("declare");

    let listed = repo
        .list(&scope, TENANT, TaxonomyClass::Region)
        .await
        .expect("list");
    assert_eq!(
        values(&listed),
        vec![
            ("eu", TaxonomyState::Active),
            (SEEDED_REGION, TaxonomyState::Active)
        ],
        "two rows now, the seed among them: {listed:?}"
    );
    let universe = active_regions(&conn, &scope, TENANT)
        .await
        .expect("universe");
    assert_eq!(universe.len(), 2);
    assert!(universe.contains(&Region::new(SEEDED_REGION).expect("ok")));
}

/// The seed is an ordinary value once written: retiring it leaves the universe
/// **empty** — the tenant asked for no regions — and no second seed appears.
#[tokio::test]
async fn retiring_the_seed_empties_the_universe_and_nothing_reseeds_it() {
    let (repo, scope, provider) = harness().await;
    let conn = provider.conn().expect("conn");
    let held = seeded_region();
    let retired = TaxonomyEntry {
        state: TaxonomyState::Retired,
        ..held.clone()
    };

    write_value_patch(
        &conn,
        &scope,
        TENANT,
        TaxonomyClass::Region,
        &held,
        &retired,
        None,
        stamp(),
    )
    .await
    .expect("materialise, then retire");

    let listed = repo
        .list(&scope, TENANT, TaxonomyClass::Region)
        .await
        .expect("list");
    assert_eq!(
        values(&listed),
        vec![(SEEDED_REGION, TaxonomyState::Retired)],
        "the row exists, retired - not a fresh virtual seed"
    );
    assert!(
        active_regions(&conn, &scope, TENANT)
            .await
            .expect("universe")
            .is_empty(),
        "a tenant that retired every region has the empty universe it asked for"
    );
    assert_eq!(
        region_readiness(
            &conn,
            &scope,
            TENANT,
            &Region::new(SEEDED_REGION).expect("ok")
        )
        .await
        .expect("read"),
        None
    );
}
