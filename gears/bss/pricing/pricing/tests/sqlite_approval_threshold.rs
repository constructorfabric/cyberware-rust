//! `pricing_approval_threshold` on the `SQLite` mirror — the same five CHECKs and
//! the same two triggers, executed.
//!
//! The Postgres suite (`tests/postgres_approval_threshold.rs`) is the canonical
//! one; this exists because **a missing `SQLite` constraint has twice left the
//! whole suite green**. Almost every in-crate test reaches the mirror, so a guard
//! the mirror does not carry is a guard nothing exercises: the Rust layer refuses
//! the same values, and the schema's silence is invisible behind it.
//!
//! The mirror differs from the canonical schema in exactly one way that matters
//! here, and it is in this suite's favour: `SQLite` does not enforce `varchar(3)`,
//! so `chk_pricing_approval_threshold_currency` is observable on the **long** side
//! as well as the short one, where on Postgres the type answers first.
//!
//! The staging rules are the Postgres suite's — every row is valid but for the one
//! thing under test, because `..._basis` refuses most of what its siblings refuse.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use bss_pricing::domain::instant::utc_ymd_hms;
use common::{exec, migrated_db, must_succeed, scalar};
use sea_orm::DatabaseConnection;
use time::OffsetDateTime;

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const ACTOR: &str = "44444444-4444-4444-4444-444444444444";
const AT: &str = "'2099-01-01T00:00:00.000Z'";

/// Reject, **and for the stated reason**.
///
/// Per-suite, and sharpened the way `tests/sqlite_append_only.rs` sharpens its
/// own: every CHECK, trigger and key over this table carries
/// `pricing_approval_threshold`, so the table name alone would be satisfied by any
/// of them and says nothing about which guard answered.
async fn must_be_rejected(conn: &DatabaseConnection, sql: &str, because: &str) {
    let err = exec(conn, sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("the guard `{because}` must reject: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains("pricing_approval_threshold"),
        "the rejection must name the table it came from, got: {message}"
    );
    assert!(
        message.contains(because),
        "the rejection must be the one under test (`{because}`), got: {message}"
    );
}

fn entry(overrides: &[(&str, &str)]) -> String {
    let mut columns: Vec<(&str, String)> = vec![
        ("tenant_id", format!("'{TENANT}'")),
        ("version", "0".to_owned()),
        ("currency", "'USD'".to_owned()),
        ("absolute_minor", "50000".to_owned()),
        ("percent_bp", "NULL".to_owned()),
        ("effective_from", AT.to_owned()),
        ("created_by", format!("'{ACTOR}'")),
    ];
    for (name, value) in overrides {
        match columns.iter_mut().find(|(column, _)| column == name) {
            Some(slot) => (*value).clone_into(&mut slot.1),
            None => columns.push((name, (*value).to_owned())),
        }
    }
    let names = columns
        .iter()
        .map(|(column, _)| *column)
        .collect::<Vec<_>>()
        .join(", ");
    let values = columns
        .iter()
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO pricing_approval_threshold ({names}) VALUES ({values})")
}

/// The whitelist half: both bases land on the mirror, so every refusal below is a
/// refusal of the one thing it changed.
#[tokio::test]
async fn a_well_formed_entry_lands_on_each_basis() {
    let conn = migrated_db().await;
    must_succeed(&conn, &entry(&[])).await;
    must_succeed(
        &conn,
        &entry(&[
            ("currency", "'EUR'"),
            ("absolute_minor", "NULL"),
            ("percent_bp", "500"),
        ]),
    )
    .await;
}

/// §6's `{absolute_minor | percent}` is a choice: neither is not one.
#[tokio::test]
async fn a_threshold_entry_with_neither_basis_is_refused() {
    let conn = migrated_db().await;
    must_be_rejected(
        &conn,
        &entry(&[("absolute_minor", "NULL"), ("percent_bp", "NULL")]),
        "chk_pricing_approval_threshold_basis",
    )
    .await;
}

/// And both is not one either.
#[tokio::test]
async fn a_threshold_entry_with_both_bases_is_refused() {
    let conn = migrated_db().await;
    must_be_rejected(
        &conn,
        &entry(&[("absolute_minor", "50000"), ("percent_bp", "500")]),
        "chk_pricing_approval_threshold_basis",
    )
    .await;
}

/// `absolute_minor ≥ 0`, with `percent_bp` left NULL so the basis constraint is
/// not the one answering.
#[tokio::test]
async fn a_negative_absolute_threshold_is_refused() {
    let conn = migrated_db().await;
    must_be_rejected(
        &conn,
        &entry(&[("absolute_minor", "-1")]),
        "chk_pricing_approval_threshold_absolute_non_negative",
    )
    .await;
    must_succeed(&conn, &entry(&[("absolute_minor", "0")])).await;
}

/// `percent > 0`, with `absolute_minor` left NULL for the mirror reason.
#[tokio::test]
async fn a_non_positive_percent_threshold_is_refused() {
    let conn = migrated_db().await;
    for bp in ["0", "-1"] {
        must_be_rejected(
            &conn,
            &entry(&[("absolute_minor", "NULL"), ("percent_bp", bp)]),
            "chk_pricing_approval_threshold_percent_positive",
        )
        .await;
    }
    must_succeed(
        &conn,
        &entry(&[("absolute_minor", "NULL"), ("percent_bp", "1")]),
    )
    .await;
}

/// The ISO 4217 shape, on both sides — the mirror does not enforce `varchar(3)`,
/// so the four-character code the canonical schema's type refuses reaches this
/// CHECK here and is the sharper half of the proof.
#[tokio::test]
async fn a_currency_of_the_wrong_length_is_refused() {
    let conn = migrated_db().await;
    for code in ["'US'", "''", "'USDX'"] {
        must_be_rejected(
            &conn,
            &entry(&[("currency", code)]),
            "chk_pricing_approval_threshold_currency",
        )
        .await;
    }
}

/// `version >= 0`.
#[tokio::test]
async fn a_negative_version_is_refused() {
    let conn = migrated_db().await;
    must_be_rejected(
        &conn,
        &entry(&[("version", "-1")]),
        "chk_pricing_approval_threshold_version",
    )
    .await;
    must_succeed(&conn, &entry(&[("version", "0")])).await;
}

/// `trg_pricing_approval_threshold_no_delete`, executed.
///
/// And the row is read back afterwards, which is the other half: a trigger that
/// aborted and a statement that took effect anyway are indistinguishable from the
/// error alone.
#[tokio::test]
async fn a_deleted_threshold_version_is_refused() {
    let conn = migrated_db().await;
    must_succeed(&conn, &entry(&[])).await;
    must_be_rejected(
        &conn,
        &format!("DELETE FROM pricing_approval_threshold WHERE tenant_id = '{TENANT}'"),
        "append-only history",
    )
    .await;
    assert_eq!(
        scalar(
            &conn,
            "SELECT CAST(count(*) AS text) AS v FROM pricing_approval_threshold",
        )
        .await,
        "1",
        "the refused DELETE must leave the version where it was"
    );
}

/// `trg_pricing_approval_threshold_no_update`, executed, with the same read-back.
#[tokio::test]
async fn an_updated_threshold_version_is_refused() {
    let conn = migrated_db().await;
    must_succeed(&conn, &entry(&[])).await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE pricing_approval_threshold SET absolute_minor = 1 WHERE tenant_id = '{TENANT}'"
        ),
        "is immutable",
    )
    .await;
    assert_eq!(
        scalar(
            &conn,
            "SELECT CAST(absolute_minor AS text) AS v FROM pricing_approval_threshold",
        )
        .await,
        "50000",
        "the refused UPDATE must leave the threshold where it was"
    );
}

/// The mirror dropped the pair too, and the rebuild kept everything else.
///
/// `SQLite` cannot `DROP COLUMN` a column a CHECK names, so the migration rebuilds
/// `pricing_policy_object` — which is the statement most likely to lose a column,
/// a default or a constraint in transcription. The surviving CHECK is executed, not
/// merely named.
#[tokio::test]
async fn the_policy_objects_old_threshold_pair_is_gone_and_the_rebuild_kept_the_rest() {
    let conn = migrated_db().await;
    for column in ["approval_threshold_minor", "approval_threshold_currency"] {
        let err = exec(
            &conn,
            &format!("SELECT {column} FROM pricing_policy_object"),
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("pricing_policy_object must no longer carry {column}"));
        assert!(
            err.to_string().contains(column),
            "the error must name the dropped column, got: {err}"
        );
    }
    // Every other column survived the rebuild, defaults included.
    must_succeed(
        &conn,
        &format!("INSERT INTO pricing_policy_object (tenant_id, updated_by) VALUES ('{TENANT}', '{ACTOR}')"),
    )
    .await;
    // The witness is `tax_display_policy_mode`, not the `tax_display_mode` D-240
    // retired: this case asserts the table's defaults, and this is the column a
    // hand-written column list is likeliest to drop, since it belongs to Slice 4's
    // switch rather than to the policy object's original shape.
    assert_eq!(
        scalar(
            &conn,
            "SELECT tax_display_policy_mode AS v FROM pricing_policy_object",
        )
        .await,
        "fail_closed",
        "the rebuild must keep the fail-safe default"
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT CAST(enforced_migration_notice_days AS text) AS v FROM pricing_policy_object",
        )
        .await,
        "60",
        "the rebuild must keep D-49's floor as the default"
    );
    // And a surviving CHECK still refuses, so the rebuild carried the constraints
    // and not only the columns.
    let err = exec(
        &conn,
        &format!(
            "INSERT INTO pricing_policy_object (tenant_id, updated_by, \
             enforced_migration_notice_days) VALUES ('{ACTOR}', '{ACTOR}', 59)"
        ),
    )
    .await
    .expect_err("the sixty-day floor must survive the rebuild");
    assert!(
        err.to_string()
            .contains("chk_pricing_policy_object_notice_floor"),
        "the rebuilt table must carry the floor by name, got: {err}"
    );
}

/// **L-8: one version, one instant — asserted, because nothing in the schema says so.**
///
/// `effective_from` is a column on every entry row and the primary key is
/// `(tenant_id, version, currency)`, so two rows of one version *can* hold two
/// instants. `threshold_repo::read_version` used to take the instant off
/// `rows.first()` — a silent choice among them, made in `ORDER BY currency` order, so
/// the answer would have depended on which currency sorted first and the pin a
/// reviewer signed would have been taken over one of two values.
///
/// **The state is staged through the repository, not with raw SQL**, and that is the
/// finding rather than a convenience: `open_version` called twice on one version number
/// with two currency sets is exactly the mutation the store deliberately permits —
/// `infra::threshold`'s module doc names it, *"an `INSERT` of a currency the version did
/// not have is the one mutation of an approved version the store permits"* — so the
/// disagreement is reachable through this crate's own API and not only by writing round
/// it.
#[tokio::test]
async fn a_version_whose_rows_disagree_about_their_instant_is_a_corrupt_row() {
    use bss_pricing::infra::storage::repo::threshold_repo;

    let tenant = TENANT.parse::<uuid::Uuid>().expect("a uuid");
    let scope = toolkit_db::secure::AccessScope::for_tenant(tenant);
    let stamp = bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: ACTOR.parse().expect("a uuid"),
        recorded_at: at_utc(1),
        correlation_id: uuid::Uuid::from_u128(0x_c0_11),
    };
    let provider = migrated_provider().await;
    let conn = provider.conn().expect("a scoped connection");

    threshold_repo::open_version(
        &conn,
        &scope,
        tenant,
        0,
        at_utc(1),
        &[row_for("AUD")],
        stamp,
    )
    .await
    .expect("the version's first entry lands");
    // The same version number, a currency it did not have, **a different instant**.
    threshold_repo::open_version(
        &conn,
        &scope,
        tenant,
        0,
        at_utc(2),
        &[row_for("BRL")],
        stamp,
    )
    .await
    .expect("the store permits widening a version, which is the whole hazard");

    let err = threshold_repo::read_version(&conn, &scope, tenant, 0)
        .await
        .expect_err("a version with two instants is not a version");

    match err {
        bss_pricing::infra::storage::RepoError::CorruptRow(detail) => {
            assert!(
                detail.contains("pricing_approval_threshold"),
                "the refusal names the table it came from: {detail}"
            );
            assert!(
                detail.contains("AUD") || detail.contains("BRL"),
                "and the entry that disagrees, which is what an operator looks at: {detail}"
            );
        }
        other => panic!("expected a corrupt row, got: {other:?}"),
    }

    // The control: one instant across the set, and the read answers with both entries.
    let provider = migrated_provider().await;
    let conn = provider.conn().expect("a scoped connection");
    threshold_repo::open_version(
        &conn,
        &scope,
        tenant,
        0,
        at_utc(1),
        &[row_for("AUD"), row_for("BRL")],
        stamp,
    )
    .await
    .expect("a well-formed version lands");
    let held = threshold_repo::read_version(&conn, &scope, tenant, 0)
        .await
        .expect("a well-formed version reads")
        .expect("version 0 is there");
    assert_eq!(held.entries.len(), 2, "both entries, one instant");
    assert_eq!(held.effective_from, at_utc(1));
}

/// A `DBProvider` over a migrated mirror — the raw-SQL helpers above hand back a
/// `sea_orm` connection, which the repository layer deliberately cannot take.
async fn migrated_provider() -> toolkit_db::DBProvider<toolkit_db::DbError> {
    let db = toolkit_db::connect_db("sqlite::memory:", toolkit_db::ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    toolkit_db::migration_runner::run_migrations_for_testing(
        &db,
        <bss_pricing::infra::storage::migrations::Migrator as sea_orm_migration::MigratorTrait>::migrations(),
    )
    .await
    .expect("run the gear migrator");
    toolkit_db::DBProvider::<toolkit_db::DbError>::new(db)
}

/// One entry row on `currency`, absolute basis.
fn row_for(currency: &str) -> bss_pricing::infra::storage::repo::ThresholdEntryRow {
    bss_pricing::infra::storage::repo::ThresholdEntryRow {
        currency: currency.to_owned(),
        absolute_minor: Some(50_000),
        percent_bp: None,
    }
}

/// The fixture instants, on 2099 by this suite's convention.
fn at_utc(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 1, day, 0, 0, 0)
}

/// **A version the store cannot read does not take the tenant's whole policy down
/// with it.**
///
/// The walk's own contract is that a version which no longer matches what its
/// approver signed is **skipped** — `infra::threshold`'s module doc says the pin
/// match "is what makes that widening take the version out of effect instead of
/// quietly extending what an approver signed". A version whose rows disagree about
/// their instant is the same situation one step earlier: it is certainly not the
/// tenant's approved policy. But `effective_version` read it with `?`, so
/// `RepoError::CorruptRow` became `DomainError::Internal` and propagated — and
/// because the walk is greatest-first, one unreadable version **aborted the whole
/// walk**. Every publish, every window mutation and the policy `GET` answered 500
/// for that tenant, permanently, with no in-gear remedy: the table refuses `UPDATE`
/// and `DELETE`, so nothing could clear it.
///
/// The staging is the store's own permitted mutation, not a write around it —
/// `(tenant, version, currency)` is the primary key, so an `INSERT` of a currency
/// the version did not have is accepted, and a second instant comes with it. That is
/// how two proposals that both minted version 0 with disjoint currency sets leave
/// the store.
///
/// Skipping is the fail-safe direction and the whole point: with no readable
/// approved version the tenant has no policy, `evaluate` answers
/// `noConfiguredThreshold`, and **everything is material**. A 500 protects nobody
/// and stops the remedy.
#[tokio::test]
async fn a_version_the_store_cannot_read_is_skipped_rather_than_failing_the_whole_walk() {
    use bss_pricing::infra::storage::repo::threshold_repo;

    let tenant = TENANT.parse::<uuid::Uuid>().expect("a uuid");
    let scope = toolkit_db::secure::AccessScope::for_tenant(tenant);
    let stamp = bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: ACTOR.parse().expect("a uuid"),
        recorded_at: at_utc(1),
        correlation_id: uuid::Uuid::from_u128(0x_c0_12),
    };
    let provider = migrated_provider().await;
    let conn = provider.conn().expect("a scoped connection");

    threshold_repo::open_version(
        &conn,
        &scope,
        tenant,
        0,
        at_utc(1),
        &[row_for("AUD")],
        stamp,
    )
    .await
    .expect("the version's first entry lands");
    threshold_repo::open_version(
        &conn,
        &scope,
        tenant,
        0,
        at_utc(2),
        &[row_for("BRL")],
        stamp,
    )
    .await
    .expect("the store permits the widening; that is the hazard, not the defect");

    // The precondition, so the assertion below is not vacuous: version 0 really is
    // unreadable, which is what used to abort the walk.
    let read = threshold_repo::read_version(&conn, &scope, tenant, 0).await;
    assert!(
        read.is_err(),
        "the staging has to produce an unreadable version or this case proves nothing: {read:?}"
    );

    let effective = bss_pricing::infra::threshold::effective_policy(&conn, &scope, tenant).await;

    assert!(
        matches!(effective, Ok(None)),
        "an unreadable version is skipped and the tenant simply has no policy - which makes \
         every change material. It must not be an error: {effective:?}"
    );
}

// ---------------------------------------------------------------------------
// D-185: the tombstone table, and the one sequence it shares.
// ---------------------------------------------------------------------------

/// One tombstone row, valid unless an override makes it otherwise.
fn tombstone(overrides: &[(&str, &str)]) -> String {
    let mut columns: Vec<(&str, String)> = vec![
        ("tenant_id", format!("'{TENANT}'")),
        ("version", "0".to_owned()),
        ("effective_from", AT.to_owned()),
        ("created_by", format!("'{ACTOR}'")),
    ];
    for (name, value) in overrides {
        match columns.iter_mut().find(|(column, _)| column == name) {
            Some(slot) => (*value).clone_into(&mut slot.1),
            None => columns.push((name, (*value).to_owned())),
        }
    }
    let names = columns
        .iter()
        .map(|(column, _)| *column)
        .collect::<Vec<_>>()
        .join(", ");
    let values = columns
        .iter()
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO pricing_approval_threshold_tombstone ({names}) VALUES ({values})")
}

/// The whitelist half, and the property the entry table cannot express: a version
/// with **no currency at all**.
#[tokio::test]
async fn a_well_formed_tombstone_lands_and_holds_one_row_per_version() {
    let conn = migrated_db().await;
    must_succeed(&conn, &tombstone(&[])).await;
    // One tombstone per version, by the primary key: a second row for version 0 would
    // be a retirement with two authored instants and no answer to which one an
    // approver signed.
    // The primary key by its columns, not the bare table name: `must_be_rejected`'s
    // own doc says every CHECK, trigger and key over this table carries
    // `pricing_approval_threshold`, so the table name is satisfied by the version
    // CHECK and by either append-only trigger just as well as by the key this case
    // is about.
    must_be_rejected(
        &conn,
        &tombstone(&[("effective_from", "'2099-06-01T00:00:00.000Z'")]),
        "pricing_approval_threshold_tombstone.version",
    )
    .await;
    // And the refusal wrote nothing: the first row's instant still stands, which is
    // the read-back the two trigger cases below carry.
    assert_eq!(
        scalar(
            &conn,
            "SELECT CAST(effective_from AS text) AS v FROM \
             pricing_approval_threshold_tombstone WHERE version = 0",
        )
        .await,
        AT.trim_matches('\''),
        "a refused second tombstone must leave the first one's instant alone"
    );
    must_succeed(&conn, &tombstone(&[("version", "1")])).await;
}

/// `version >= 0` on the tombstone table too — one sequence, one rule.
#[tokio::test]
async fn a_negative_tombstone_version_is_refused() {
    let conn = migrated_db().await;
    must_be_rejected(
        &conn,
        &tombstone(&[("version", "-1")]),
        "chk_pricing_approval_threshold_tombstone_version",
    )
    .await;
    must_succeed(&conn, &tombstone(&[("version", "0")])).await;
}

/// `trg_pricing_approval_threshold_tombstone_no_delete`, executed, with the read-back
/// its sibling's proof has.
#[tokio::test]
async fn a_deleted_tombstone_is_refused() {
    let conn = migrated_db().await;
    must_succeed(&conn, &tombstone(&[])).await;
    must_be_rejected(
        &conn,
        &format!("DELETE FROM pricing_approval_threshold_tombstone WHERE tenant_id = '{TENANT}'"),
        "append-only history",
    )
    .await;
    assert_eq!(
        scalar(
            &conn,
            "SELECT CAST(count(*) AS text) AS v FROM pricing_approval_threshold_tombstone",
        )
        .await,
        "1",
        "the refused DELETE must leave the retirement where it was - it is what an approval pin \
         covers"
    );
}

/// `trg_pricing_approval_threshold_tombstone_no_update`, executed.
///
/// The column under test is `effective_from`, which is **when the two-person rule
/// comes back**: an operator who could move it after a reviewer signed would move the
/// one fact the retirement is about.
#[tokio::test]
async fn an_updated_tombstone_is_refused() {
    let conn = migrated_db().await;
    must_succeed(&conn, &tombstone(&[])).await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE pricing_approval_threshold_tombstone SET effective_from = \
             '2099-06-01T00:00:00.000Z' WHERE tenant_id = '{TENANT}'"
        ),
        "is immutable",
    )
    .await;
    assert_eq!(
        scalar(
            &conn,
            "SELECT effective_from AS v FROM pricing_approval_threshold_tombstone",
        )
        .await,
        "2099-01-01T00:00:00.000Z",
        "the refused UPDATE must leave the instant where it was"
    );
}

/// **One version sequence across two tables**, through the repository that joins them.
///
/// The line that makes D-185 executable rather than merely stored. Three properties,
/// and each of them is a distinct way to get the join wrong:
///
/// * `latest_version` sees the tombstone — blind to it, the next proposal mints the
///   number the tombstone already holds, and one version ends up carrying both an
///   authored retirement and an authored entry set;
/// * `versions_desc` includes it, **greatest first across the merge** — absent from the
///   list, `effective_version` never visits it, so an approved retirement can never be
///   in force and the tenant keeps the thresholds they asked to be rid of, which is the
///   fail-**open** direction;
/// * `read_version` returns it as a version whose entry set is *empty*, which is what
///   distinguishes it from `None` — a version nobody proposed.
#[tokio::test]
async fn the_two_threshold_tables_are_one_version_sequence() {
    use bss_pricing::infra::storage::repo::threshold_repo;

    let tenant = TENANT.parse::<uuid::Uuid>().expect("a uuid");
    let scope = toolkit_db::secure::AccessScope::for_tenant(tenant);
    let stamp = bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: ACTOR.parse().expect("a uuid"),
        recorded_at: at_utc(1),
        correlation_id: uuid::Uuid::from_u128(0x_c0_13),
    };
    let provider = migrated_provider().await;
    let conn = provider.conn().expect("a scoped connection");

    // Version 0: an entry set. Version 1: a tombstone. Interleaved on purpose, so a
    // merge that trusted the two `ORDER BY`s' relative order is caught.
    threshold_repo::open_version(
        &conn,
        &scope,
        tenant,
        0,
        at_utc(1),
        &[row_for("AUD")],
        stamp,
    )
    .await
    .expect("the entry version lands");
    threshold_repo::open_tombstone(&conn, &scope, tenant, 1, at_utc(2), stamp)
        .await
        .expect("the tombstone lands");

    assert_eq!(
        threshold_repo::latest_version(&conn, &scope, tenant)
            .await
            .expect("the latest version reads"),
        Some(1),
        "the tombstone consumed a number, so the next proposal is 2 and not 1 again"
    );
    assert_eq!(
        threshold_repo::versions_desc(&conn, &scope, tenant)
            .await
            .expect("the version list reads"),
        vec![1, 0],
        "one sequence, greatest first - the walk's whole contract"
    );

    let retired = threshold_repo::read_version(&conn, &scope, tenant, 1)
        .await
        .expect("a tombstone reads")
        .expect("version 1 is there, which is exactly what a zero-row version could not be");
    assert!(
        retired.entries.is_empty(),
        "a tombstone is the version that says none: {retired:?}"
    );
    assert_eq!(
        retired.effective_from,
        at_utc(2),
        "and it carries its own authored instant, so the pinned subject is reconstructible"
    );
    assert!(
        threshold_repo::read_version(&conn, &scope, tenant, 2)
            .await
            .expect("an unknown version reads")
            .is_none(),
        "a version nobody proposed is still `None`, which is the distinction the tombstone \
         exists to make"
    );

    // And the entry version beside it is unchanged: a tombstone supersedes by being
    // later, never by editing what came before.
    let configured = threshold_repo::read_version(&conn, &scope, tenant, 0)
        .await
        .expect("the entry version reads")
        .expect("version 0 is there");
    assert_eq!(configured.entries.len(), 1);
    assert_eq!(configured.effective_from, at_utc(1));
}

/// **A version that is both a tombstone and an entry set is a version no approver
/// signed, and it is refused rather than resolved.**
///
/// The two tables have two primary keys and neither sees the other, so nothing in
/// either schema refuses this — and it is reachable: two proposals that read one
/// `latest_version` and mint one number, one retiring and one configuring, collide on
/// nothing. A trigger could refuse it only by querying the sibling table on every
/// insert of either, which puts the invariant somewhere no reader of either table would
/// look for it.
///
/// So `read_version` refuses, exactly as it refuses a version whose rows disagree about
/// their instant — and picking one of the two would be worse than either: the
/// retirement's reviewer signed the empty digest and the entry set's reviewer signed the
/// entry digest, so whichever was chosen would be one signature applied to content its
/// signer was not shown. Refused, the version is skipped by
/// `infra::approval::read_threshold_version` and the tenant stays on the version they
/// already had — neither proposal takes effect.
#[tokio::test]
async fn a_version_that_is_both_a_tombstone_and_an_entry_set_is_a_corrupt_row() {
    use bss_pricing::infra::storage::repo::threshold_repo;

    let tenant = TENANT.parse::<uuid::Uuid>().expect("a uuid");
    let scope = toolkit_db::secure::AccessScope::for_tenant(tenant);
    let stamp = bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: ACTOR.parse().expect("a uuid"),
        recorded_at: at_utc(1),
        correlation_id: uuid::Uuid::from_u128(0x_c0_14),
    };
    let provider = migrated_provider().await;
    let conn = provider.conn().expect("a scoped connection");

    threshold_repo::open_version(
        &conn,
        &scope,
        tenant,
        0,
        at_utc(1),
        &[row_for("AUD")],
        stamp,
    )
    .await
    .expect("the entry version lands");
    threshold_repo::open_tombstone(&conn, &scope, tenant, 0, at_utc(1), stamp)
        .await
        .expect("neither table refuses the other's version number; that is the hazard");

    let err = threshold_repo::read_version(&conn, &scope, tenant, 0)
        .await
        .expect_err("a version that is both is not a version");
    match err {
        bss_pricing::infra::storage::RepoError::CorruptRow(detail) => {
            assert!(
                detail.contains("pricing_approval_threshold"),
                "the refusal names the table it came from: {detail}"
            );
            assert!(
                detail.contains("tombstone"),
                "and says which of the two states it could not choose between: {detail}"
            );
        }
        other => panic!("expected a corrupt row, got: {other:?}"),
    }

    // The fail-safe direction: the ambiguous version takes effect as nothing, and with
    // no other version approved the tenant has no policy - which makes everything
    // material.
    let effective = bss_pricing::infra::threshold::effective_policy(&conn, &scope, tenant).await;
    assert!(
        matches!(effective, Ok(None)),
        "the ambiguous version is skipped, not raised: {effective:?}"
    );
}

/// **A lost version mint is a retriable conflict, not a storage failure.**
///
/// The caller mints the version number off `latest_version` and then inserts, so two
/// proposals racing on one tenant can both read the same number and the physical key
/// decides which lands. That is the ordinary optimistic shape and its loser wants
/// *retry* — `CONCURRENT_MUTATION`, 409.
///
/// Both writes here mapped **every** failure to `RepoError::Db` until 2026-08-18, so
/// the loser was answered a 500: "this gear is broken", over a request that was well
/// formed and would succeed on the next attempt. `approval_repo::open` sits on the
/// same policy plane one file over and classifies the identical shape through
/// `policy_guard_or_contention` — two policy-plane writes disagreeing about whether a
/// lost mint is a conflict or a crash.
///
/// The variant is the assertion and not the status: `Db` and `ConcurrentMutation` both
/// exist, and a test that only checked "an error came back" passed before the fix.
#[tokio::test]
async fn a_duplicate_version_number_is_a_conflict_and_not_a_storage_failure() {
    use bss_pricing::infra::storage::RepoError;
    use bss_pricing::infra::storage::repo::threshold_repo;

    let tenant = TENANT.parse::<uuid::Uuid>().expect("a uuid");
    let scope = toolkit_db::secure::AccessScope::for_tenant(tenant);
    let stamp = bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: ACTOR.parse().expect("a uuid"),
        recorded_at: at_utc(1),
        correlation_id: uuid::Uuid::from_u128(0x_c0_12),
    };
    let provider = migrated_provider().await;
    let conn = provider.conn().expect("a scoped connection");

    threshold_repo::open_version(
        &conn,
        &scope,
        tenant,
        0,
        at_utc(1),
        &[row_for("EUR")],
        stamp,
    )
    .await
    .expect("the first mint lands");

    // The same `(tenant, version, currency)`: the primary key the racing loser hits.
    let refused = threshold_repo::open_version(
        &conn,
        &scope,
        tenant,
        0,
        at_utc(1),
        &[row_for("EUR")],
        stamp,
    )
    .await
    .expect_err("the second mint of one number is refused");
    assert!(
        matches!(refused, RepoError::ConcurrentMutation { .. }),
        "a lost mint is retriable, not a crash: {refused:?}"
    );

    // The tombstone half is the same write on the other table, and had the same
    // defect; without this the fix could be applied to one of the two and look done.
    threshold_repo::open_tombstone(&conn, &scope, tenant, 1, at_utc(2), stamp)
        .await
        .expect("the first tombstone lands");
    let refused = threshold_repo::open_tombstone(&conn, &scope, tenant, 1, at_utc(2), stamp)
        .await
        .expect_err("the second tombstone of one number is refused");
    assert!(
        matches!(refused, RepoError::ConcurrentMutation { .. }),
        "{refused:?}"
    );
}
