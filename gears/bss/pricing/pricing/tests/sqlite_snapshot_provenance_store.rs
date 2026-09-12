//! What the **store** refuses of a `migrated-origin` record — Slice 11's frozen
//! snapshot (`design/11-lifecycle.md` §6, `inst-sy-provenance`, `inst-sy-freeze`,
//! `inst-sy-select`, `inst-sy-payload`, D-76, D-81, D-87, D-102), driven as raw
//! SQL because what is under test is the store's own guarantee and not any
//! repository's use of it.
//!
//! # Why a suite at all, when the table is already written in production
//!
//! `synthesis_repo::freeze_or_load` writes it and `rest_migrated_origin_snapshots`
//! drives that path end to end — and neither can see any rule here stop
//! refusing. A repository only ever offers **legal** values: its `NewProvenance`
//! carries a typed `Trigger`, a resolved set it built itself and a payload it
//! materialized, so every statement it emits satisfies all four `CHECK`s by
//! construction. A suite driving the repo catches a constraint that got
//! *narrower*; it is blind to one that got dropped, which is the failure mode
//! this gear denounces and the reason every rule below is driven past the
//! repository.
//!
//! Until this file the table appeared in the whole test tree **only** in the two
//! migration censuses, which pin names and digests and run no statement.
//!
//! # What the frozen arms cost the rest of the suite, stated rather than found
//!
//! Every `CHECK` here is reachable **only on `INSERT`**. There is no legal
//! `UPDATE` of this table at all — `trg_pricing_snapshot_provenance_no_update` is
//! unconditional and fires before any constraint is evaluated — so a case that
//! tried to move a row into an illegal shape would be answered by the trigger and
//! would prove nothing about the constraint it named. The absence of such cases
//! below is structural, not an oversight.
//!
//! # Where this arm and its Postgres twin genuinely differ
//!
//! `postgres_schema_snapshot_provenance` proves the same roster on the engine
//! that ships. Two differences are real rather than cosmetic, and each is pinned
//! on the side that has it:
//!
//! * **`json_valid` is load-bearing here and has no Postgres counterpart.** The
//!   mirror's `resolved` and `payload` are `text`, so a writer can put anything
//!   at all in them and the first conjunct of each shape `CHECK` is what refuses;
//!   on the shipping engine the columns are `jsonb` and the **type** refuses the
//!   same statement before any constraint runs.
//! * **The one PL/pgSQL function is two triggers here**, because `SQLite` has no
//!   `BEFORE UPDATE OR DELETE`. Two arms are two things to lose, so both are
//!   driven separately below.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm::DatabaseConnection;

mod common;

use common::{exec, migrated_db, must_succeed, scalar};

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const OTHER_TENANT: &str = "22222222-2222-2222-2222-222222222222";
const SUBSCRIPTION: &str = "33333333-3333-3333-3333-333333333333";
const OTHER_SUBSCRIPTION: &str = "44444444-4444-4444-4444-444444444444";
const PLAN: &str = "55555555-5555-5555-5555-555555555555";
const ACTOR: &str = "66666666-6666-6666-6666-666666666666";
const PROVENANCE: &str = "77777777-7777-7777-7777-777777777777";
const OTHER_PROVENANCE: &str = "88888888-8888-8888-8888-888888888888";

/// D-81's instant `t`, and the commit instant. Two columns rather than one
/// because §6 keeps "as of when this was resolved" and "when this was written"
/// apart, and the migration deliberately declares **no** relation between them:
/// a `migration`-trigger `t` is the migration's effective timestamp, which is
/// routinely in the run-up future of the row that records it.
const SNAPSHOT_INSTANT: &str = "2026-11-05T00:00:00+00:00";
const CREATED_AT: &str = "2026-08-07T10:00:00+00:00";

/// One resolved row, in `inst-sy-select`'s shape: the id and the tier it came
/// from. The tier is D-76's, and it is what tells an auditor a real published
/// price from a governed backdated reconstruction.
const RESOLVED: &str =
    r#"[{"priceId":"99999999-9999-9999-9999-999999999999","source":"live_history"}]"#;

/// `inst-sy-payload`'s materialized content, reduced to the one field that makes
/// it an object: what the shape `CHECK` states is that it **is** one, not what is
/// in it.
const PAYLOAD: &str = r#"{"model_kind":"flat"}"#;

/// The columns a freeze carries, as SQL literals. Every case below is this row
/// with exactly **one** field moved, which is what makes each refusal the
/// constraint it names rather than something else about the statement.
#[derive(Clone)]
struct Freeze {
    provenance: &'static str,
    tenant: &'static str,
    subscription: &'static str,
    /// `NULL` or an integer — a bare SQL literal, because tier 2's absent
    /// revision is a value this suite has to be able to write.
    revision: &'static str,
    trigger: &'static str,
    resolved: &'static str,
    payload: &'static str,
}

/// The row `synthesis_repo` would write: tier 1, `migration` trigger, one
/// resolved row, an object payload.
fn freeze() -> Freeze {
    Freeze {
        provenance: PROVENANCE,
        tenant: TENANT,
        subscription: SUBSCRIPTION,
        revision: "3",
        trigger: "migration",
        resolved: RESOLVED,
        payload: PAYLOAD,
    }
}

fn insert(row: &Freeze) -> String {
    let Freeze {
        provenance,
        tenant,
        subscription,
        revision,
        trigger,
        resolved,
        payload,
    } = row;
    format!(
        "INSERT INTO pricing_snapshot_provenance \
         (provenance_id, tenant_id, subscription_ref, source_plan_id, source_revision, \
          snapshot_instant, trigger_kind, acting_principal, resolved, payload, created_at) \
         VALUES ('{provenance}', '{tenant}', '{subscription}', '{PLAN}', {revision}, \
          '{SNAPSHOT_INSTANT}', '{trigger}', '{ACTOR}', '{resolved}', '{payload}', \
          '{CREATED_AT}')"
    )
}

async fn must_be_rejected(conn: &DatabaseConnection, sql: &str, because: &str) {
    let err = exec(conn, sql)
        .await
        .expect_err(&format!("this statement must be rejected: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains(because),
        "the rejection must be the one under test (`{because}`), got: {message}"
    );
}

/// How many records the tenant holds for the fixture's subscription.
///
/// Load-bearing rather than decorative: a `BEFORE` trigger that stopped raising
/// leaves a statement that silently takes effect, and a case asserting only "the
/// statement did not fail" cannot tell that from a refusal.
async fn frozen_records(conn: &DatabaseConnection) -> String {
    scalar(
        conn,
        &format!(
            "SELECT CAST(count(*) AS TEXT) AS v FROM pricing_snapshot_provenance \
             WHERE tenant_id = '{TENANT}' AND subscription_ref = '{SUBSCRIPTION}'"
        ),
    )
    .await
}

/// The whitelist half: the row `inst-sy-freeze` actually writes lands, and lands
/// whole. Every refusal below departs from exactly this statement, so without
/// this case a fixture that had drifted into being illegal for some *other*
/// reason would make every case here pass while proving nothing.
#[tokio::test]
async fn a_synthesized_snapshot_freezes_and_reads_back_whole() {
    let conn = migrated_db().await;
    must_succeed(&conn, &insert(&freeze())).await;
    assert_eq!(frozen_records(&conn).await, "1");
    assert_eq!(
        scalar(
            &conn,
            &format!(
                "SELECT payload AS v FROM pricing_snapshot_provenance \
                 WHERE provenance_id = '{PROVENANCE}'"
            )
        )
        .await,
        PAYLOAD,
        "the payload is what Rating evaluates from -- there is no CatalogVersion \
         to fall back to on a migrated-origin ref"
    );
}

/// **A revision above the old `integer` column's range freezes and reads back** —
/// Z6-7, and the one case in this suite that goes *through* the repository.
///
/// The module doc's rule is that every constraint here is driven past
/// `synthesis_repo`, because a suite driving the repository is blind to a rule that
/// got dropped. This case is the exception and the reason is that its subject is
/// not a store constraint at all: on `SQLite`, `integer` and `bigint` are one type
/// — INTEGER affinity, up to 8 bytes — so no raw statement can distinguish the
/// column before `pricing_snapshot_provenance` from the column after it, and a raw probe would
/// have been green against the defect. What was narrow was the **Rust** side:
/// `freeze_or_load` guarded with `i32::try_from` and answered `CorruptRow` for a
/// revision a plan's own `bigint` column can hold, and the entity typed the field
/// `Option<i32>`. Both are only reachable through the repository.
///
/// The value is `i32::MAX as u64 + 1`, the old boundary exactly: one less passed
/// before the widening too.
#[tokio::test]
async fn a_revision_beyond_the_old_columns_range_round_trips() {
    use bss_pricing::domain::scope_key::PlanId;
    use bss_pricing::domain::synthesis::SynthesisTrigger;
    use bss_pricing::infra::storage::migrations::Migrator;
    use bss_pricing::infra::storage::repo::{NewProvenance, synthesis_repo};

    use sea_orm_migration::MigratorTrait as _;
    use toolkit_db::secure::AccessScope;
    use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
    use uuid::Uuid;

    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    toolkit_db::migration_runner::run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    let conn = provider.conn().expect("scoped connection");

    let tenant_id = Uuid::parse_str(TENANT).expect("a tenant uuid");
    let beyond = u64::try_from(i32::MAX).expect("i32::MAX is non-negative") + 1;
    let instant = utc_ymd_hms(2026, 11, 5, 0, 0, 0);

    let frozen = synthesis_repo::freeze_or_load(
        &conn,
        &AccessScope::for_tenant(tenant_id),
        NewProvenance {
            provenance_id: Uuid::parse_str(PROVENANCE).expect("a provenance uuid"),
            tenant_id,
            subscription_ref: Uuid::parse_str(SUBSCRIPTION).expect("a subscription uuid"),
            source_plan_id: PlanId::new(Uuid::parse_str(PLAN).expect("a plan uuid")),
            source_revision: Some(beyond),
            snapshot_instant: instant,
            trigger: SynthesisTrigger::Migration,
            acting_principal: Uuid::parse_str(ACTOR).expect("an actor uuid"),
            resolved: serde_json::from_str(RESOLVED).expect("the resolved fixture is json"),
            payload: serde_json::from_str(PAYLOAD).expect("the payload fixture is json"),
            created_at: instant,
        },
    )
    .await
    .expect("a revision above the old integer column's range must be storable");

    assert!(frozen.created);
    assert_eq!(
        frozen.record.source_revision,
        Some(beyond),
        "the revision must survive the round trip, not merely the insert"
    );
}

/// **The stored trigger vocabulary is D-81's two and the underscored spelling is
/// the stored one.**
///
/// `inst-sy-freeze` writes the pair hyphenated (`first-rating`) because that is
/// prose; the column stores `first_rating`. A writer that passed the design
/// set's spelling straight through is exactly what this refuses, and a third
/// trigger — a re-synthesis, a correction — is the case the constraint exists
/// for: D-81 gives each trigger a different instant `t`, so a third one would
/// freeze a third price with no rule saying which one rating reads.
#[tokio::test]
async fn the_trigger_is_one_of_d81s_two() {
    let conn = migrated_db().await;
    for trigger in ["first-rating", "correction", ""] {
        must_be_rejected(
            &conn,
            &insert(&Freeze {
                trigger,
                ..freeze()
            }),
            "chk_pricing_snapshot_provenance_trigger",
        )
        .await;
    }

    // Both sanctioned values land, on their own subscriptions -- the whitelist
    // half, without which a constraint narrowed to one trigger would pass.
    must_succeed(&conn, &insert(&freeze())).await;
    must_succeed(
        &conn,
        &insert(&Freeze {
            provenance: OTHER_PROVENANCE,
            subscription: OTHER_SUBSCRIPTION,
            trigger: "first_rating",
            ..freeze()
        }),
    )
    .await;
}

/// A revision is an ordinal, and its **absence** is tier 2 rather than a defect.
///
/// D-87 states the case plainly: a fully-legacy key may have no plan revision at
/// all, so the column admits `NULL` and the payload is what makes the row
/// evaluable. A constraint tightened to `NOT NULL` would refuse every tier-2
/// synthesis, which is the half of D-76 that has no other store — so the `NULL`
/// insert below is as load-bearing as the refusal above it.
#[tokio::test]
async fn a_revision_is_an_ordinal_or_tier_twos_absence() {
    let conn = migrated_db().await;
    must_be_rejected(
        &conn,
        &insert(&Freeze {
            revision: "-1",
            ..freeze()
        }),
        "chk_pricing_snapshot_provenance_revision",
    )
    .await;

    must_succeed(
        &conn,
        &insert(&Freeze {
            revision: "NULL",
            ..freeze()
        }),
    )
    .await;
    // 0 is the first revision a plan mints, so the boundary is admitted and not
    // an off-by-one away from being refused.
    must_succeed(
        &conn,
        &insert(&Freeze {
            provenance: OTHER_PROVENANCE,
            subscription: OTHER_SUBSCRIPTION,
            revision: "0",
            ..freeze()
        }),
    )
    .await;
}

/// **`inst-sy-select` clause (3) is fail-closed, and an empty resolved set is
/// that refusal having been ignored.**
///
/// Synthesis never guesses a price: when neither tier resolves a row it fails
/// into the migration exception list. A row frozen with nothing resolved is a
/// snapshot rating would charge from and find empty.
///
/// **Two of the three conjuncts are independently reachable and the third is
/// shadowed, which is a fact about this engine rather than about the rule.**
/// `json_valid` is reachable (nothing else here refuses text that is not JSON at
/// all) and `json_array_length(...) > 0` is reachable (the empty array is valid
/// JSON of the right type) — but `json_type(resolved) = 'array'` can never be the
/// sole refuser, because `SQLite`'s `json_array_length` answers **0 for every
/// non-array**, so the length conjunct already refuses everything the type
/// conjunct does. The object case below is therefore armed against the
/// constraint, not against that conjunct. The shipping engine has the same shape
/// for the opposite reason: there `jsonb_array_length` *raises* on a non-array,
/// so dropping the type test turns a constraint violation into a bare runtime
/// error rather than an acceptance.
#[tokio::test]
async fn a_frozen_snapshot_resolved_at_least_one_row() {
    let conn = migrated_db().await;
    // Not JSON at all. The column is `text` on this engine, so this is a
    // statement a writer can really emit and `json_valid` is really what stops
    // it; the shipping engine's `jsonb` column refuses it one layer lower.
    must_be_rejected(
        &conn,
        &insert(&Freeze {
            resolved: "not json",
            ..freeze()
        }),
        "chk_pricing_snapshot_provenance_resolved",
    )
    .await;
    // Valid JSON of the wrong shape: an object where §6 keeps a list of resolved
    // ids, which is what a writer serializing one row instead of a set produces.
    must_be_rejected(
        &conn,
        &insert(&Freeze {
            resolved: r#"{"priceId":"99999999-9999-9999-9999-999999999999"}"#,
            ..freeze()
        }),
        "chk_pricing_snapshot_provenance_resolved",
    )
    .await;
    // The array, empty. Only the third conjunct refuses this one.
    must_be_rejected(
        &conn,
        &insert(&Freeze {
            resolved: "[]",
            ..freeze()
        }),
        "chk_pricing_snapshot_provenance_resolved",
    )
    .await;
}

/// The payload is an **object** — `inst-sy-payload`'s materialized row content,
/// which Rating reads field by field. An array or a bare scalar is a payload
/// nothing can evaluate, and this table is the only place the content lives:
/// there is no `CatalogVersion` behind a `migrated-origin` ref to re-resolve it
/// from.
#[tokio::test]
async fn the_payload_is_an_object() {
    let conn = migrated_db().await;
    for payload in ["not json", "[]", r#"["flat"]"#, "7"] {
        must_be_rejected(
            &conn,
            &insert(&Freeze {
                payload,
                ..freeze()
            }),
            "chk_pricing_snapshot_provenance_payload",
        )
        .await;
    }
}

/// **§9's idempotency, as an index rather than as a convention: one subscription,
/// one frozen snapshot, ever.**
///
/// The second insert below carries a *different* trigger, which is the drift the
/// index is aimed at: keyed on `(subscription_ref, trigger)` instead, the
/// `migration` and `first-rating` triggers would each freeze their own — and
/// D-81 gives those two different instants `t`, so the subscription would hold
/// two different frozen prices with no rule saying which one rating reads.
#[tokio::test]
async fn one_subscription_holds_one_frozen_snapshot_ever() {
    let conn = migrated_db().await;
    must_succeed(&conn, &insert(&freeze())).await;
    must_be_rejected(
        &conn,
        &insert(&Freeze {
            provenance: OTHER_PROVENANCE,
            trigger: "first_rating",
            ..freeze()
        }),
        "UNIQUE constraint failed: pricing_snapshot_provenance.tenant_id, \
         pricing_snapshot_provenance.subscription_ref",
    )
    .await;

    // The index is tenant-scoped, like every read of this table. Stated because
    // the assertion above would also pass against an index over
    // `subscription_ref` alone, and that index would be a different rule.
    must_succeed(
        &conn,
        &insert(&Freeze {
            provenance: OTHER_PROVENANCE,
            tenant: OTHER_TENANT,
            ..freeze()
        }),
    )
    .await;
}

/// The primary key, driven by a **different** subscription so the idempotency
/// index above cannot be what answers. Two records sharing a `provenance_id`
/// would give one audit reference two payloads.
#[tokio::test]
async fn two_records_cannot_share_a_provenance_id() {
    let conn = migrated_db().await;
    must_succeed(&conn, &insert(&freeze())).await;
    must_be_rejected(
        &conn,
        &insert(&Freeze {
            subscription: OTHER_SUBSCRIPTION,
            ..freeze()
        }),
        "UNIQUE constraint failed: pricing_snapshot_provenance.provenance_id",
    )
    .await;
}

/// **The snapshot is frozen, and `UPDATE` is refused whatever it touches.**
///
/// A `migrated-origin` ref resolves through **no** `CatalogVersion` by
/// construction (D-87, Foundation §4.4), so the immutability every other consumer
/// contract gets from a frozen `CatalogVersion` has to come from this row. If it
/// could be edited, a disputed legacy charge could be re-explained after the fact
/// by the party being disputed with.
///
/// Driven column by column rather than once: the trigger is unconditional today,
/// and a `WHEN` clause narrowing it to the payload would leave the instant, the
/// trigger and the resolved set editable with a single-statement case still
/// green.
#[tokio::test]
async fn a_frozen_snapshot_is_never_edited() {
    let conn = migrated_db().await;
    must_succeed(&conn, &insert(&freeze())).await;
    for assignment in [
        r#"payload = '{"model_kind":"tiered"}'"#,
        r#"resolved = '[{"priceId":"99999999-9999-9999-9999-999999999999","source":"historical_import"}]'"#,
        "snapshot_instant = '2020-01-01T00:00:00+00:00'",
        "trigger_kind = 'first_rating'",
        "source_revision = 4",
        "source_plan_id = '12121212-1212-1212-1212-121212121212'",
        "acting_principal = '13131313-1313-1313-1313-131313131313'",
        "created_at = '2020-01-01T00:00:00+00:00'",
    ] {
        must_be_rejected(
            &conn,
            &format!(
                "UPDATE pricing_snapshot_provenance SET {assignment} \
                 WHERE provenance_id = '{PROVENANCE}'"
            ),
            "a migrated-origin snapshot is frozen",
        )
        .await;
    }

    assert_eq!(
        scalar(
            &conn,
            &format!(
                "SELECT payload AS v FROM pricing_snapshot_provenance \
                 WHERE provenance_id = '{PROVENANCE}'"
            )
        )
        .await,
        PAYLOAD,
        "and the refusal must have held -- a row that was rewritten anyway is \
         the failure this table exists to prevent"
    );
}

/// `DELETE` is refused for the same reason one step further: an auditor
/// reconstructing a legacy charge needs the record to still exist, and a
/// subscription's snapshot outlives the migration that synthesized it.
///
/// The count is read back because a `BEFORE DELETE` trigger that returned rather
/// than raising would cancel the statement **silently**, and a case asserting
/// only a refusal cannot tell that from one that swept the row away.
#[tokio::test]
async fn a_frozen_snapshot_is_never_deleted() {
    let conn = migrated_db().await;
    must_succeed(&conn, &insert(&freeze())).await;
    must_be_rejected(
        &conn,
        &format!("DELETE FROM pricing_snapshot_provenance WHERE provenance_id = '{PROVENANCE}'"),
        "DELETE of a migrated-origin record is not permitted",
    )
    .await;
    assert_eq!(
        frozen_records(&conn).await,
        "1",
        "the record must still be there for the auditor who comes looking"
    );
}
