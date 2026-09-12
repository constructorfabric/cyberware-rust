//! The segmented audit chain against a real database.
//!
//! What a chain is for is that somebody can walk it years later and recompute
//! every link. So the load-bearing test here is the **re-walk**: read a
//! segment's rows back out of the columns that stored them, rebuild each
//! record's canonical encoding, and check the digest the writer put in
//! `row_hash`. A test that only asserted "a hash was written" would keep passing
//! after the encoding and the store had stopped agreeing.
//!
//! **And here is the limit of that, stated because it is easy to misread
//! (measured 2026-08-04, while building the Postgres suite).** The re-walk
//! rebuilds each preimage **from the stored columns**, so it is evidence about
//! *connectedness* and about the encoding — never about *content*. Blank
//! `before_state` and `after_state` in both the hashed record and the stored row
//! and this suite stays **fully green**: it is self-consistent by construction,
//! because the data it checks is the data it derives its expectation from. That
//! is the Phase-2 G8 defect exactly, still live here, and the reason
//! `tests/postgres_audit_chain.rs::every_row_holds_the_record_its_writer_was_handed_and_hashes_to_it`
//! exists — it asserts the content against an independently constructed
//! expectation and names the field. **Connectedness and content need different
//! tests, and the second never substitutes for the first.**
//!
//! # What is owed, and what **kind** of thing each item is
//!
//! An earlier version of this list said "unproven and owed to a Postgres suite"
//! of five properties at once. That was wrong about three of them, and the
//! wrongness was not cosmetic: one entry — "the isolation the publish
//! transaction is opened with behaves as its CONTRACT claims" — was not a claim
//! awaiting evidence at all. It was checkable by reading two lines of this
//! crate's own code together, and it was **false**. Filing it as owed-to-Postgres
//! converted an argument that could have been finished into a debt nobody would
//! collect, and that is how a live concurrency defect stayed hidden. So each
//! entry now says what it is.
//!
//! 1. **Unprovable here.** Two concurrent mutations of *different* `chain_id`s
//!    of one tenant both commit without contending on a chain head. This is the
//!    entire benefit D-135 bought — before segmentation one head serialized every
//!    audited mutation of a tenant inside its own mutation transaction — and
//!    `05-governance.md` §9 lists it as an integration acceptance criterion.
//!    `sqlite::memory:` serializes writers, so nothing here can observe it.
//!    Fairly owed to a Postgres suite.
//! 2. **Undemonstrated, not unproven.** Two mutations of the *same* aggregate
//!    serialize rather than fork. Both halves of the mechanism are already
//!    established: the key half by
//!    [`a_second_row_at_one_position_is_refused_by_the_primary_key`] below, and
//!    the rollback half by `in_transaction`'s own contract. What Postgres would
//!    add is the concurrent *demonstration*, not the guarantee — so listing the
//!    whole property as unproven overstates the gap.
//! 3. ~~**Unimplemented, and owed to a decision rather than to a suite.**~~
//!    **Both halves paid, and this entry had gone stale in two directions**
//!    (corrected 2026-08-04). The decision arrived first — D-159 named the class
//!    `CONCURRENT_MUTATION`, and the insert has routed through
//!    `contention_or_db` since, so "the refusal stays `RepoError::Db`" was
//!    already false when written; this file's own comment block further down was
//!    up to date while this one was not, so the file disagreed with itself. The
//!    suite arrived on 2026-08-04:
//!    `tests/postgres_audit_chain.rs::the_loser_of_a_same_segment_race_is_a_retriable_contention`
//!    drives two real transactions to the collision and asserts the whole ladder
//!    through to the 409.
//! 4. **Was false, and is now corrected.** "The isolation level the publish
//!    transaction is opened with behaves as its CONTRACT claims" used to sit
//!    here. The CONTRACT's key argument was sound and its predicate conclusion
//!    was not, and the gap it left is closed by the per-row version guard in
//!    `price_repo::publish_rows` — a predicate on a statement, provable here and
//!    proved. What remains genuinely unobservable is only that `SQLite` cannot
//!    distinguish one isolation level from another, which is a statement about
//!    the backend rather than a debt.
//! 5. **Half already proven; the other half is a cost measurement.** The
//!    registry round-trip inside an open transaction. Its *failure* behaviour is
//!    proven by `registry_absence_stops_the_publish_and_writes_nothing` in
//!    `tests/sqlite_publish_commit.rs`. Only its *cost* — what holding a network
//!    call inside a transaction does to a real connection pool — is owed, and
//!    that is a load test rather than a correctness suite.
//!
//! The template for item 1 is
//! `gears/bss/ledger/ledger/tests/postgres_chain.rs`, whose
//! `concurrent_posts_form_linear_chain` is the shape it wants. It is owed by
//! whichever group first stands a Postgres suite up.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::audit::{
    AuditAction, AuditRecord, AuditSubjectKind, audit_row_hash, genesis_prev_hash,
};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::audit_log;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{NewAuditEntry, audit_repo};
use time::OffsetDateTime;

use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, Order};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

mod common;

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e_22);
const CHAIN: Uuid = Uuid::from_u128(0x9_1a4);
const OTHER_CHAIN: Uuid = Uuid::from_u128(0x9_1a5);
const ACTOR: Uuid = Uuid::from_u128(0xac_10);

async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

/// A vector instant carrying **sub-second** precision.
///
/// Deliberately not `hh:00:00`. The digest hashes `timestamp_micros()`, and this
/// suite's whole purpose is byte fidelity through storage — so a vector set on
/// the second would leave the one column whose round trip could silently lose
/// precision (`timestamptz` on Postgres, `text` on `SQLite`) untested by the
/// re-walk that exists to catch exactly that.
fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, 17, 42)
        .checked_add(time::Duration::microseconds(123_456))
        .expect("a fixed instant plus a fixed offset")
}

fn entry(tenant_id: Uuid, chain_id: Uuid, subject: &str, hour: u32) -> NewAuditEntry {
    NewAuditEntry {
        tenant_id,
        chain_id,
        recorded_at: at(hour),
        actor_principal_id: ACTOR,
        action: AuditAction::Publish,
        subject_kind: AuditSubjectKind::PlanRevision,
        subject_ref: subject.to_owned(),
        before_state: Some(json!({"lifecycleState": "draft", "rowVersion": 0})),
        after_state: Some(json!({"lifecycleState": "published", "rowVersion": 1})),
        approval_ref: None,
        correlation_id: Uuid::from_u128(0xc0_11),
    }
}

/// One segment's rows, in chain order.
async fn segment(
    db: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    chain_id: Uuid,
) -> Vec<audit_log::Model> {
    let conn = db.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(audit_log::Column::TenantId.eq(tenant_id))
                .add(audit_log::Column::ChainId.eq(chain_id)),
        )
        .order_by(audit_log::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the segment")
}

/// Rebuild one stored row's canonical encoding and recompute its digest.
///
/// This is the verification job in miniature, and it reads **only** the columns
/// — nothing is carried over from the write. A field the writer hashed and the
/// store dropped, or a field the store holds and the encoding no longer covers,
/// fails here.
fn recompute(row: &audit_log::Model, prev_hash: &[u8; 32]) -> [u8; 32] {
    let seq = u64::try_from(row.seq).expect("a non-negative seq");
    let record = AuditRecord {
        tenant_id: row.tenant_id,
        chain_id: row.chain_id,
        seq,
        recorded_at: row.recorded_at,
        actor_principal_id: row.actor_principal_id,
        action: *AuditAction::ALL
            .iter()
            .find(|action| action.as_str() == row.action)
            .expect("a declared action"),
        subject_kind: *AuditSubjectKind::ALL
            .iter()
            .find(|kind| kind.as_str() == row.subject_kind)
            .expect("a declared subject kind"),
        subject_ref: &row.subject_ref,
        before_state: row.before_state.as_ref(),
        after_state: row.after_state.as_ref(),
        approval_ref: row.approval_ref,
        correlation_id: row.correlation_id,
    };
    audit_row_hash(&record, prev_hash).expect("the stored record canonicalizes")
}

/// Walk a segment from its genesis, checking every link and every digest.
fn walk(rows: &[audit_log::Model], tenant_id: Uuid, chain_id: Uuid) {
    let mut expected_prev = genesis_prev_hash(tenant_id, chain_id);
    for (position, row) in rows.iter().enumerate() {
        assert_eq!(
            u64::try_from(row.seq).expect("a non-negative seq"),
            u64::try_from(position).expect("a small segment"),
            "a segment's seq is dense and starts at 0"
        );
        assert_eq!(
            row.prev_hash.as_deref(),
            Some(expected_prev.as_slice()),
            "row {position} does not link to its predecessor"
        );
        let recomputed = recompute(row, &expected_prev);
        assert_eq!(
            row.row_hash.as_slice(),
            recomputed.as_slice(),
            "row {position} does not reproduce its own digest"
        );
        expected_prev = recomputed;
    }
}

// ---------------------------------------------------------------------------
// Genesis and linkage.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_segments_first_row_is_seq_zero_and_carries_its_genesis_seed() {
    let db = provider().await;
    let scope = AccessScope::for_tenant(TENANT);
    let conn = db.conn().expect("conn");

    let seq = audit_repo::append(&conn, &scope, entry(TENANT, CHAIN, "plan/0", 10))
        .await
        .expect("append the first record");

    assert_eq!(seq, 0);
    let rows = segment(&db, &scope, TENANT, CHAIN).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].prev_hash.as_deref(),
        Some(genesis_prev_hash(TENANT, CHAIN).as_slice()),
        "the first link is the segment's seed, never NULL"
    );
    assert_eq!(rows[0].row_hash.len(), 32);
}

#[tokio::test]
async fn every_row_links_to_its_predecessor_and_reproduces_its_own_digest() {
    let db = provider().await;
    let scope = AccessScope::for_tenant(TENANT);
    let conn = db.conn().expect("conn");
    for revision in 0..4_u32 {
        audit_repo::append(
            &conn,
            &scope,
            entry(TENANT, CHAIN, &format!("plan/{revision}"), 10 + revision),
        )
        .await
        .expect("append");
    }

    walk(&segment(&db, &scope, TENANT, CHAIN).await, TENANT, CHAIN);
}

#[tokio::test]
async fn two_segments_of_one_tenant_are_independent_in_content() {
    let db = provider().await;
    let scope = AccessScope::for_tenant(TENANT);
    let conn = db.conn().expect("conn");
    audit_repo::append(&conn, &scope, entry(TENANT, CHAIN, "plan-a/0", 10))
        .await
        .expect("append to the first segment");
    audit_repo::append(&conn, &scope, entry(TENANT, OTHER_CHAIN, "plan-b/0", 10))
        .await
        .expect("append to the second segment");
    audit_repo::append(&conn, &scope, entry(TENANT, CHAIN, "plan-a/1", 11))
        .await
        .expect("extend the first segment");

    let first = segment(&db, &scope, TENANT, CHAIN).await;
    let second = segment(&db, &scope, TENANT, OTHER_CHAIN).await;

    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].seq, 0, "each segment starts at 0 of its own");
    assert_ne!(
        first[0].prev_hash, second[0].prev_hash,
        "a genesis bound to the tenant alone would give both segments one seed, \
         and a whole segment could then be transplanted onto another aggregate"
    );
    walk(&first, TENANT, CHAIN);
    walk(&second, TENANT, OTHER_CHAIN);
    // Neither segment's rows appear in the other's walk.
    for row in &second {
        assert!(
            !first
                .iter()
                .any(|other| other.subject_ref == row.subject_ref)
        );
    }
}

#[tokio::test]
async fn one_chain_id_under_two_tenants_gets_two_seeds() {
    let db = provider().await;
    let mine = AccessScope::for_tenant(TENANT);
    let theirs = AccessScope::for_tenant(OTHER_TENANT);
    let conn = db.conn().expect("conn");
    audit_repo::append(&conn, &mine, entry(TENANT, CHAIN, "plan/0", 10))
        .await
        .expect("append mine");
    audit_repo::append(&conn, &theirs, entry(OTHER_TENANT, CHAIN, "plan/0", 10))
        .await
        .expect("append theirs");

    let mine_rows = segment(&db, &mine, TENANT, CHAIN).await;
    let their_rows = segment(&db, &theirs, OTHER_TENANT, CHAIN).await;

    assert_eq!(mine_rows.len(), 1);
    assert_eq!(their_rows.len(), 1);
    assert_ne!(mine_rows[0].prev_hash, their_rows[0].prev_hash);
    assert_ne!(mine_rows[0].row_hash, their_rows[0].row_hash);
}

#[tokio::test]
async fn another_tenants_segment_is_invisible() {
    let db = provider().await;
    let mine = AccessScope::for_tenant(TENANT);
    let theirs = AccessScope::for_tenant(OTHER_TENANT);
    let conn = db.conn().expect("conn");
    audit_repo::append(&conn, &theirs, entry(OTHER_TENANT, CHAIN, "plan/0", 10))
        .await
        .expect("append theirs");

    assert!(
        segment(&db, &mine, OTHER_TENANT, CHAIN).await.is_empty(),
        "SecureORM keeps another tenant's trail out of this one's reach"
    );

    // And their segment does not move my own chain's head: my first append is
    // still `seq = 0`.
    let seq = audit_repo::append(&conn, &mine, entry(TENANT, CHAIN, "plan/0", 10))
        .await
        .expect("append mine");
    assert_eq!(seq, 0);
}

// ---------------------------------------------------------------------------
// Tamper evidence.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_append_only_trigger_rejects_update_and_delete() {
    // The physical half of `inst-au-tamper`: there is no sanctioned in-place
    // mutation of an audit record at all, so unlike `pricing_plan` and
    // `pricing_price` there is no whitelist here — a hash chain whose links can
    // be rewritten is not evidence.
    let conn = common::migrated_db().await;
    common::must_succeed(
        &conn,
        "INSERT INTO pricing_audit_log (
             tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
             action, subject_kind, subject_ref, row_hash)
         VALUES ('t1','c1',0,'mutation','2026-08-03T12:00:00Z','a1',
                 'publish','plan_revision','plan/0', x'00')",
    )
    .await;

    let updated = common::exec(
        &conn,
        "UPDATE pricing_audit_log SET subject_ref = 'plan/9' WHERE seq = 0",
    )
    .await
    .expect_err("UPDATE must be rejected");
    assert!(updated.to_string().contains("append-only"), "got {updated}");

    let deleted = common::exec(&conn, "DELETE FROM pricing_audit_log WHERE seq = 0")
        .await
        .expect_err("DELETE must be rejected");
    assert!(deleted.to_string().contains("append-only"), "got {deleted}");
}

#[tokio::test]
async fn a_row_whose_content_no_longer_matches_its_digest_is_caught_by_the_walk() {
    // The trigger above is what stops a row being rewritten *through* the
    // database, so a tampered row is one that reached the table around this
    // gear. What the walk gives is detection: recomputing a record whose actor
    // has been changed produces a digest the stored `row_hash` does not match,
    // and every later link fails with it.
    let db = provider().await;
    let scope = AccessScope::for_tenant(TENANT);
    let conn = db.conn().expect("conn");
    audit_repo::append(&conn, &scope, entry(TENANT, CHAIN, "plan/0", 10))
        .await
        .expect("append");
    let rows = segment(&db, &scope, TENANT, CHAIN).await;

    let mut tampered = rows[0].clone();
    tampered.actor_principal_id = Uuid::from_u128(0xdead);
    let recomputed = recompute(&tampered, &genesis_prev_hash(TENANT, CHAIN));

    assert_ne!(
        rows[0].row_hash.as_slice(),
        recomputed.as_slice(),
        "a changed actor must not reproduce the stored digest"
    );
}

#[tokio::test]
async fn a_second_row_at_one_position_is_refused_by_the_primary_key() {
    // The property the whole CONTRACT rests on: two writers who read one head
    // and both insert at `seq + 1` cannot both succeed, so the chain is linear
    // by construction at any isolation level. This asserts the key's half of it;
    // the concurrent half is owed to a Postgres suite (see the module doc).
    let conn = common::migrated_db().await;
    common::must_succeed(
        &conn,
        "INSERT INTO pricing_audit_log (
             tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
             action, subject_kind, subject_ref, row_hash)
         VALUES ('t1','c1',0,'mutation','2026-08-03T12:00:00Z','a1',
                 'publish','plan_revision','plan/0', x'00')",
    )
    .await;

    let collision = common::exec(
        &conn,
        "INSERT INTO pricing_audit_log (
             tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
             action, subject_kind, subject_ref, row_hash)
         VALUES ('t1','c1',0,'mutation','2026-08-03T12:00:00Z','a2',
                 'publish','plan_revision','plan/0', x'01')",
    )
    .await
    .expect_err("two rows at one position must be impossible");
    // **By the key that refused.** Every `CHECK`, both append-only triggers and
    // this primary key spell `pricing_audit_log` in their messages and several can
    // answer one statement, so a bare `expect_err` passes with the primary key
    // deleted and a neighbour refusing instead. `SQLite` prints the column list.
    let message = collision.to_string();
    assert!(
        message.contains("UNIQUE constraint failed"),
        "a second row at one position collides on the key, got: {message}"
    );
    for column in [
        "pricing_audit_log.tenant_id",
        "pricing_audit_log.chain_id",
        "pricing_audit_log.seq",
    ] {
        assert!(
            message.contains(column),
            "the key is (tenant_id, chain_id, seq) and `{column}` is part of it, got: {message}"
        );
    }
}

#[tokio::test]
async fn every_subject_kind_d158_declares_is_storable_in_the_trail() {
    // D-158's claim is about the **declaration**, not about who writes it: the two
    // stores spell one enumeration and extend it in step. This is the audit-plane
    // twin of `sqlite_approval_repo::every_subject_kind_d158_declares_is_storable_on_the_mirror`,
    // and it is what keeps `chk_pricing_audit_log_subject_kind` (Z6-6,
    // `chk_pricing_audit_log_subject_kind`) in step with `AuditSubjectKind::ALL`: an eighth variant
    // reddens here on the next `cargo test`, with no route, no service and no
    // writer having been touched.
    //
    // Iterated off `ALL` rather than spelled out, because a hand-written list of
    // the same seven tokens is a second roster to keep true — the shape the
    // constraint itself exists to remove.
    let conn = common::migrated_db().await;

    for (seq, kind) in AuditSubjectKind::ALL.iter().enumerate() {
        let token = kind.as_str();
        common::must_succeed(
            &conn,
            &format!(
                "INSERT INTO pricing_audit_log (
                     tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
                     action, subject_kind, subject_ref, row_hash)
                 VALUES ('t1','c1',{seq},'mutation','2026-08-03T12:00:00Z','a1',
                         'publish','{token}','subject/0', x'00')"
            ),
        )
        .await;
    }
}

#[tokio::test]
async fn a_subject_kind_outside_d158s_enumeration_is_refused_by_the_store() {
    // The other direction, and the one that makes the constraint falsifiable: the
    // trail is append-only, hash-chained and retained for seven-plus years, so a
    // token that arrives unspelled cannot be corrected afterwards — an `UPDATE`
    // breaks the chain and a `DELETE` re-writes it, and both are refused by the
    // table's own triggers. Before `chk_pricing_audit_log_subject_kind` this insert succeeded: the
    // column was `text NOT NULL` and nothing more, on either engine, while its
    // `pricing_approval` sibling had been CHECK-constrained since `000015`.
    let conn = common::migrated_db().await;

    let refused = common::exec(
        &conn,
        "INSERT INTO pricing_audit_log (
             tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
             action, subject_kind, subject_ref, row_hash)
         VALUES ('t1','c1',0,'mutation','2026-08-03T12:00:00Z','a1',
                 'publish','plan-revision','subject/0', x'00')",
    )
    .await
    .expect_err(
        "a subject_kind outside AuditSubjectKind::ALL must be refused: 'plan-revision' is the \
         hyphenated near-miss of a real token, which is exactly what a hand-spelled writer \
         produces",
    );
    assert!(
        refused
            .to_string()
            .contains("chk_pricing_audit_log_subject_kind"),
        "and by that constraint rather than by one of the four beside it, got: {refused}"
    );
}

/// Refused, **and by the named constraint**.
///
/// Naming it is what makes the case a proof. Every `CHECK`, the primary key and
/// both append-only triggers on this table spell `pricing_audit_log` in their
/// messages, and several of them can answer one statement — so a case that
/// accepted any error would pass with the guard under test deleted, refused
/// instead by a neighbour it never meant to trip. `SQLite` reports a named
/// `CHECK` as `CHECK constraint failed: <name>`.
async fn must_violate(conn: &DatabaseConnection, sql: &str, constraint: &str) {
    let err = common::exec(conn, sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("the constraint must refuse: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains("CHECK constraint failed"),
        "the refusal must come from a CHECK, got: {message}"
    );
    assert!(
        message.contains(constraint),
        "the refusal must be `{constraint}`, got: {message}"
    );
}

#[tokio::test]
async fn every_action_the_domain_declares_is_storable_in_the_trail() {
    // The positive control for `chk_pricing_audit_log_action`, and the half that
    // makes it falsifiable in the narrowing direction: a constraint minted from a
    // subset of the enum would refuse an act the domain can perform, and the
    // refusing case below would still pass. Iterated off `ALL` rather than
    // spelled out, because a hand-written copy of the same tokens is a second
    // roster to keep true — the shape the constraint exists to remove.
    let conn = common::migrated_db().await;

    for (seq, action) in AuditAction::ALL.iter().enumerate() {
        let token = action.as_str();
        common::must_succeed(
            &conn,
            &format!(
                "INSERT INTO pricing_audit_log (
                     tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
                     action, subject_kind, subject_ref, row_hash)
                 VALUES ('t1','c1',{seq},'mutation','2026-08-03T12:00:00Z','a1',
                         '{token}','plan_revision','subject/0', x'00')"
            ),
        )
        .await;
    }
}

#[tokio::test]
async fn an_action_outside_the_domains_enumeration_is_refused_by_the_store() {
    // The trail is append-only, hash-chained and retained for seven-plus years,
    // so a token that arrives unspelled cannot be corrected afterwards: an
    // `UPDATE` breaks the chain and a `DELETE` re-writes it, and both are refused
    // by the table's own triggers.
    //
    // `window.cancel` is the shape a free column collects — one act spelled per
    // call site, dotted here and bare in `AuditAction::Abandon`'s own token — and
    // no query over `action` can reconcile the two.
    let conn = common::migrated_db().await;

    must_violate(
        &conn,
        "INSERT INTO pricing_audit_log (
             tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
             action, subject_kind, subject_ref, row_hash)
         VALUES ('t1','c1',0,'mutation','2026-08-03T12:00:00Z','a1',
                 'window.cancel','plan_revision','subject/0', x'00')",
        "chk_pricing_audit_log_action",
    )
    .await;
}

#[tokio::test]
async fn a_mutation_row_carrying_segment_heads_is_impossible() {
    // `chk_pricing_audit_log_rollup` is what keeps the roll-up — which nothing
    // in this gear writes — from being confusable with a mutation record.
    let conn = common::migrated_db().await;

    let heads = common::exec(
        &conn,
        "INSERT INTO pricing_audit_log (
             tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
             action, subject_kind, subject_ref, segment_heads, row_hash)
         VALUES ('t1','c1',0,'mutation','2026-08-03T12:00:00Z','a1',
                 'publish','plan_revision','plan/0', '{}', x'00')",
    )
    .await
    .expect_err("a mutation row may not carry segment heads");
    assert!(
        heads.to_string().contains("chk_pricing_audit_log_rollup"),
        "and by D-158's rollup constraint rather than by a neighbour, got: {heads}"
    );

    // **The other direction of the biconditional.** The constraint is
    // `(entry_kind = 'rollup') = (segment_heads IS NOT NULL)`, so a rollup row
    // *without* heads is refused by the same object - and a CHECK written as the
    // one-way implication a mutation row's refusal alone would satisfy admits it.
    let headless = common::exec(
        &conn,
        "INSERT INTO pricing_audit_log (
             tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
             action, subject_kind, subject_ref, row_hash)
         VALUES ('t1','c1',1,'rollup','2026-08-03T12:00:00Z','a1',
                 'publish','plan_revision','plan/0', x'01')",
    )
    .await
    .expect_err("a rollup row must carry the segment heads it rolls up");
    assert!(
        headless
            .to_string()
            .contains("chk_pricing_audit_log_rollup"),
        "and by the same constraint from the other side, got: {headless}"
    );
}

// ---------------------------------------------------------------------------
// The loser of a same-segment race is a retriable contention (D-159)
// ---------------------------------------------------------------------------

// The chain-head contention itself is **unprovable here, and provably so.** The
// head is `MAX(seq)` over the segment, so `append` always targets a free
// position on a well-formed segment - which is the module CONTRACT's own point
// ("a fork is unrepresentable"). The only way to occupy the target is a genuine
// concurrent writer, and `sqlite::memory:` serializes writers. What IS proved
// here is the other direction: that the recognition narrows the internal arm and
// does not swallow it. The recognition itself is one function at all three
// serialization points, and it is proved end to end at the two whose violation a
// single-writer suite can provoke - `sqlite_publish_commit.rs` (the outbox
// sequence) and `sqlite_plan_repo.rs` (the current-revision partial UNIQUE).

#[tokio::test]
async fn a_corrupt_head_is_still_a_fault_and_not_a_contention() {
    // The narrowing, from the other side: the recognition must narrow the
    // internal arm rather than swallow it. A head whose `row_hash` is not 32
    // bytes is an invariant breach and the caller is owed a fault, not "retry".
    let db = provider().await;
    let scope = AccessScope::for_tenant(TENANT);
    let conn = db.conn().expect("conn");
    let am = audit_log::ActiveModel {
        tenant_id: sea_orm::ActiveValue::Set(TENANT),
        chain_id: sea_orm::ActiveValue::Set(CHAIN),
        seq: sea_orm::ActiveValue::Set(0),
        entry_kind: sea_orm::ActiveValue::Set("mutation".to_owned()),
        recorded_at: sea_orm::ActiveValue::Set(at(9)),
        actor_principal_id: sea_orm::ActiveValue::Set(ACTOR),
        action: sea_orm::ActiveValue::Set("publish".to_owned()),
        subject_kind: sea_orm::ActiveValue::Set("plan_revision".to_owned()),
        subject_ref: sea_orm::ActiveValue::Set("plan/0".to_owned()),
        before_state: sea_orm::ActiveValue::Set(None),
        after_state: sea_orm::ActiveValue::Set(None),
        approval_ref: sea_orm::ActiveValue::Set(None),
        correlation_id: sea_orm::ActiveValue::Set(None),
        segment_heads: sea_orm::ActiveValue::Set(None),
        prev_hash: sea_orm::ActiveValue::Set(None),
        row_hash: sea_orm::ActiveValue::Set(vec![0_u8]),
    };
    audit_log::Entity::insert(am.clone())
        .secure()
        .scope_with_model(&scope, &am)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed a head the writer cannot link to");

    let refusal = audit_repo::append(&conn, &scope, entry(TENANT, CHAIN, "plan/1", 12))
        .await
        .expect_err("a short row hash is an invariant breach");

    assert!(
        matches!(refusal, RepoError::CorruptRow(_)),
        "not a contention: {refusal:?}"
    );
}
