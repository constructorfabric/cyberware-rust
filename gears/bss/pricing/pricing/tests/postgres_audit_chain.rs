//! `pricing_audit_log`, and the two concurrency claims the design set makes
//! about it — proved on real Postgres, because `SQLite` cannot answer either.
//!
//! `tests/postgres_migrations.rs` says the table's three CHECKs and its
//! append-only trigger reached the server; it issues no DML and is therefore
//! evidence of presence only. This suite is the other half: every guard answers
//! a statement written to trip it, and the assertion names the object the answer
//! came from.
//!
//! # Why this suite exists at all, and why it could not be a `SQLite` one
//!
//! `sqlite::memory:` **serializes writers**. Two of `audit_repo`'s CONTRACT
//! claims are statements about two writers running at once, so on the mirror
//! they can be neither confirmed nor refuted — not "probably fine", *unanswered
//! in both directions*:
//!
//! - **D-159.** Two mutations of the **same** aggregate contend on the primary
//!   key `(tenant_id, chain_id, seq)`; the loser takes a unique violation, its
//!   **whole transaction** rolls back, and it reaches a caller as
//!   `CONCURRENT_MUTATION` / **409** rather than as a 500. `sqlite_audit_chain.rs`
//!   proves the key's half by writing the occupied position directly — which is
//!   the collision *simulated*, not the collision *had*.
//! - **D-135.** Two mutations of **different** aggregates of one tenant both
//!   commit without contending on a chain head. This is the entire benefit
//!   segmentation bought: before it, one head serialized every audited mutation
//!   of a tenant inside its own mutation transaction, against a >= 50 rows/s
//!   repricing SLO. `05-governance.md` §9 lists it as an integration acceptance
//!   criterion. If it does not hold, the design's performance argument is void.
//!
//! Both are driven through [`audit_repo::append`] in two **real** concurrent
//! transactions. Neither is staged by writing a colliding row by hand: a
//! hand-written collision proves the constraint, and what is in doubt here is
//! the *scheduling*.
//!
//! ## How the race is made deterministic rather than hoped for
//!
//! A concurrency test that starts two tasks and asserts on the outcome is a coin
//! toss with a green side. Both races here are choreographed with observable
//! events only:
//!
//! 1. the winner's transaction appends and then **parks**, so its row exists and
//!    is uncommitted;
//! 2. the loser's transaction starts and appends — under READ COMMITTED its head
//!    read cannot see the winner's uncommitted row, so it targets the same
//!    position and blocks on the key;
//! 3. a third connection **observes the block** in `pg_locks` (`NOT granted`),
//!    which is what proves the loser's head read already happened;
//! 4. only then is the winner released to commit, and the loser's insert
//!    resolves into the unique violation.
//!
//! Step 3 is the load-bearing one. Without it the loser's head read could land
//! after the winner's commit, both would succeed, and the test would be green
//! about nothing. The D-135 test is the same choreography with the assertion
//! inverted: the second aggregate's transaction must **complete while the first
//! is still parked**, enforced by a timeout, so a mutation that contended would
//! hang and redden rather than quietly wait its turn.
//!
//! # What a Postgres suite has to do to be evidence
//!
//! Inherited verbatim from `tests/postgres_migrations.rs`, and each rule bit
//! here:
//!
//! **Prove a constraint by executing the statement it must refuse**, and assert
//! the error names that constraint. A suite that writes only valid rows catches
//! a constraint that got *narrower* and never one that stopped refusing.
//!
//! **Put the world in the state where the object under test is what answers.**
//! Every CHECK case below varies exactly one column of an otherwise-valid row,
//! so no neighbour can answer first.
//!
//! **Every guard must be provable by removal.** The mapping object → test is in
//! the group report; two of the objects here redden **two** tests each, and that
//! arity is deliberate rather than sloppy — `chk_pricing_audit_log_rollup` is a
//! biconditional with two independent arms, and the append-only trigger fires on
//! two different `TG_OP`s.
//!
//! # The chain's linkage: connectedness and content are two tests
//!
//! They are separated on purpose, and the reason is a Phase-2 defect worth not
//! repeating. `sqlite_audit_chain.rs`'s walk rebuilds each preimage **from the
//! stored columns it is checking**, so a writer that blanked `before_state` and
//! `after_state` would store NULLs, recompute over NULLs, and agree with itself:
//! the whole suite stays green while the record loses the two fields
//! `inst-au-complete` exists for. So here the expectation is built from the
//! values **this test handed to the writer**, and it is checked twice over —
//! once against the stored columns and once against the digest.
//!
//! Ignored by default; they need Docker. Run with
//! `cargo test -p cf-gears-bss-pricing --test postgres_audit_chain -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod pg_support;

use std::sync::Arc;
use std::time::Duration;

use bss_pricing::domain::audit::{
    AuditAction, AuditRecord, AuditSubjectKind, audit_row_hash, genesis_prev_hash,
};
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::infra::storage::entity::audit_log;
use bss_pricing::infra::storage::repo::{NewAuditEntry, audit_repo};
use bss_pricing::infra::storage::{RepoError, repo_failure};
use time::OffsetDateTime;

use pg_support::Pg;
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait, Order, Statement,
};
use serde_json::json;
use tokio::sync::Notify;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::secure::{AccessScope, SecureEntityExt, TxError};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const CHAIN: Uuid = Uuid::from_u128(0x9_1a4);
const OTHER_CHAIN: Uuid = Uuid::from_u128(0x9_1a5);
const ACTOR: Uuid = Uuid::from_u128(0xac_10);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_11);

/// How long a racer may take before the test calls the claim refuted.
///
/// Generous, because a cold container under load is slow — but **finite**, which
/// is the point: a D-135 mutation that contended would block until the other
/// transaction commits, and the only thing that distinguishes "did not contend"
/// from "waited its turn" is that it finished before the other side was
/// released.
const RACE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// The database under test
// ---------------------------------------------------------------------------

/// This test's own database on the shared server, carrying the applied chain.
///
/// **One container per binary and a `CREATE DATABASE` per test**, not a
/// container per test. This suite used to boot twelve containers per run and
/// carried a start/port retry loop for the `PortNotExposed` flake that came
/// back roughly once in eight runs; `tests/pg_support/mod.rs` owns that retry
/// now, and it runs once per binary instead of once per test. Under
/// guard-by-removal the flake was the expensive part: a spurious red is
/// indistinguishable from the second test a removal was not supposed to redden.
///
/// The database is applied through the toolkit runner under a `public,bss`
/// search path — the arrangement `postgres_migrations.rs` establishes as the one
/// production boots cleanly under.
async fn applied() -> Pg {
    Pg::applied().await
}

async fn exec(conn: &DatabaseConnection, sql: &str) -> Result<(), sea_orm::DbErr> {
    conn.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .map(|_| ())
}

/// Run one statement that must land.
///
/// As load-bearing as the refusals. Two of this table's guards are conditional
/// on `entry_kind`, and a suite that only ever saw refusals would pass against a
/// table nothing can be written to at all.
async fn must_succeed(conn: &DatabaseConnection, sql: &str) {
    exec(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("statement must succeed: {sql}\n{e}"));
}

/// Reject, **and by the named object**.
///
/// The fragment is the whole assertion. Every CHECK and the append-only trigger
/// on this table spell `pricing_audit_log` in their messages; a test that
/// accepted any error naming the table would pass with the guard it means to
/// prove switched off, refused instead by a neighbour it never intended to trip.
async fn must_be_rejected(conn: &DatabaseConnection, sql: &str, by: &str) {
    let err = exec(conn, sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("the guard `{by}` must reject: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains(by),
        "the rejection must be the one under test (`{by}`), got: {message}"
    );
}

async fn count(conn: &DatabaseConnection, sql: &str) -> i64 {
    let row = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql.to_owned(),
        ))
        .await
        .expect("run the count query")
        .expect("the count query must return a row");
    row.try_get::<i64>("", "n").expect("read the count")
}

async fn rows_in_segment(conn: &DatabaseConnection, chain_id: Uuid) -> i64 {
    count(
        conn,
        &format!(
            "SELECT count(*)::bigint AS n FROM bss.pricing_audit_log \
             WHERE tenant_id = '{TENANT}' AND chain_id = '{chain_id}'"
        ),
    )
    .await
}

/// One `INSERT` with the four columns the guards here discriminate on supplied.
///
/// Everything else is a valid mutation row, so exactly one thing is wrong in
/// each refusal below.
fn insert(chain_id: Uuid, seq: i64, entry_kind: &str, segment_heads: &str) -> String {
    format!(
        "INSERT INTO bss.pricing_audit_log (
            tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
            action, subject_kind, subject_ref, segment_heads, prev_hash, row_hash)
         VALUES ('{TENANT}', '{chain_id}', {seq}, '{entry_kind}',
            '2026-08-03 09:00:00+00', '{ACTOR}', 'publish', 'plan_revision',
            'plan/0', {segment_heads}, '\\xdeadbeef', '\\xdeadbeef')"
    )
}

// ---------------------------------------------------------------------------
// The CHECK constraints
// ---------------------------------------------------------------------------

/// The valid rows, first — the world in which every refusal below is a fact
/// about the constraint it names.
///
/// **Both `entry_kind` tokens**, including the one nothing in this gear writes.
/// `rollup` has no writer (`audit_repo`'s "what is deliberately absent"), so
/// without this row `chk_pricing_audit_log_entry_kind` would be pinned only from
/// the refusing side and a constraint narrowed to `IN ('mutation')` would sail
/// through the whole suite.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn both_entry_kinds_are_storable_in_their_own_shape() {
    let pg = applied().await;
    let conn = pg.raw().await;

    must_succeed(&conn, &insert(CHAIN, 0, "mutation", "NULL")).await;
    must_succeed(
        &conn,
        &insert(CHAIN, 1, "rollup", "'{\"heads\":[]}'::jsonb"),
    )
    .await;
    // And `seq = 0` is inside the range, not on the wrong side of it: the
    // constraint is `>= 0`, and a `> 0` typo would make every segment's genesis
    // unwritable while the negative case below still passed.
    must_succeed(&conn, &insert(OTHER_CHAIN, 0, "mutation", "NULL")).await;
}

/// `chk_pricing_audit_log_seq`.
///
/// A position is an offset from a segment's genesis, so there is no such thing
/// as one before it. The primary key would happily hold `-1`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_position_before_genesis_is_refused() {
    let pg = applied().await;
    let conn = pg.raw().await;
    must_be_rejected(
        &conn,
        &insert(CHAIN, -1, "mutation", "NULL"),
        "chk_pricing_audit_log_seq",
    )
    .await;
}

/// `chk_pricing_audit_log_entry_kind`.
///
/// The token is the only thing wrong: `segment_heads` stays NULL, so
/// `chk_pricing_audit_log_rollup` reads `false = false` and has nothing to say.
/// Had the case carried heads as well, the row would have been refused by the
/// neighbour and the test would have been evidence about the wrong object.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_entry_kind_outside_the_two_tokens_is_refused() {
    let pg = applied().await;
    let conn = pg.raw().await;
    must_be_rejected(
        &conn,
        &insert(CHAIN, 0, "snapshot", "NULL"),
        "chk_pricing_audit_log_entry_kind",
    )
    .await;
}

/// `chk_pricing_audit_log_rollup`, the arm that matters to this gear's writer.
///
/// `audit_repo` sets `segment_heads` NULL and relies on this constraint to make
/// its rows unmistakable for a roll-up's. Nothing else in the row is wrong.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_mutation_row_carrying_segment_heads_is_refused() {
    let pg = applied().await;
    let conn = pg.raw().await;
    must_be_rejected(
        &conn,
        &insert(CHAIN, 0, "mutation", "'{\"heads\":[]}'::jsonb"),
        "chk_pricing_audit_log_rollup",
    )
    .await;
}

/// The other arm of the same biconditional, which a one-sided test would miss.
///
/// `CHECK (segment_heads IS NULL OR entry_kind = 'rollup')` — the spelling a
/// hurried edit reaches for — refuses the row above and accepts this one, and a
/// roll-up that chains no heads is a tamper-evidence record with nothing in it.
/// This test is what would catch that narrowing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_rollup_row_chaining_no_heads_is_refused() {
    let pg = applied().await;
    let conn = pg.raw().await;
    must_be_rejected(
        &conn,
        &insert(CHAIN, 0, "rollup", "NULL"),
        "chk_pricing_audit_log_rollup",
    )
    .await;
}

/// One `INSERT` whose `action` is the only thing supplied, everything else valid.
fn insert_action(seq: i64, action: &str) -> String {
    format!(
        "INSERT INTO bss.pricing_audit_log (
            tenant_id, chain_id, seq, entry_kind, recorded_at, actor_principal_id,
            action, subject_kind, subject_ref, prev_hash, row_hash)
         VALUES ('{TENANT}', '{CHAIN}', {seq}, 'mutation',
            '2026-08-03 09:00:00+00', '{ACTOR}', '{action}', 'plan_revision',
            'plan/0', '\\xdeadbeef', '\\xdeadbeef')"
    )
}

/// `chk_pricing_audit_log_action`, from the admitting side.
///
/// The half that makes the constraint falsifiable in the narrowing direction: a
/// list minted from a subset of `AuditAction` would refuse an act the domain can
/// perform, and the refusing case below would still pass. Iterated off `ALL`
/// rather than spelled out — a hand-written copy of the same tokens is a second
/// roster to keep true.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_action_the_domain_declares_is_storable() {
    let pg = applied().await;
    let conn = pg.raw().await;

    for (seq, action) in AuditAction::ALL.iter().enumerate() {
        must_succeed(
            &conn,
            &insert_action(
                i64::try_from(seq).expect("the roster is short"),
                action.as_str(),
            ),
        )
        .await;
    }
}

/// `chk_pricing_audit_log_action`, from the refusing side.
///
/// `window.cancel` is the shape a free column collects: one act spelled per call
/// site, dotted here and bare in `AuditAction::Abandon`'s own token. On a trail
/// that is append-only by trigger and retained seven-plus years, neither spelling
/// can be reconciled with the other afterwards.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_action_outside_the_domains_enumeration_is_refused() {
    let pg = applied().await;
    let conn = pg.raw().await;
    must_be_rejected(
        &conn,
        &insert_action(0, "window.cancel"),
        "chk_pricing_audit_log_action",
    )
    .await;
}

// ---------------------------------------------------------------------------
// The append-only trigger — unconditional, unlike `pricing_plan`'s whitelist
// ---------------------------------------------------------------------------

/// **Every** UPDATE, including the ones a whitelist would have let through.
///
/// `pricing_plan` and `pricing_price` guard a *column whitelist*: some moves are
/// sanctioned and the trigger names the rest. Here there is no sanctioned
/// in-place mutation at all — a hash chain whose links can be rewritten is not
/// evidence — so the cases below deliberately include the two shapes a whitelist
/// would wave past: a column that carries no chain state (`correlation_id`) and
/// a self-assignment that changes nothing (`seq = seq`).
///
/// The assertion names `UPDATE` rather than just "append-only", which is the one
/// thing only Postgres can be asked: the PL/pgSQL body interpolates `TG_OP`, and
/// the `SQLite` mirror has to spell two triggers with fixed messages because
/// `RAISE(ABORT, ...)` takes a literal. A body that interpolated the wrong thing
/// would be created without complaint and fail only when it fired.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_update_is_refused_naming_the_operation() {
    let pg = applied().await;
    let conn = pg.raw().await;
    must_succeed(&conn, &insert(CHAIN, 0, "mutation", "NULL")).await;

    for change in [
        "subject_ref = 'plan/9'",
        "row_hash = '\\xcafebabe'",
        "prev_hash = NULL",
        "action = 'abandon'",
        "before_state = '{\"x\":1}'::jsonb",
        // No chain state at all — the column a whitelist would call harmless.
        "correlation_id = '00000000-0000-0000-0000-0000000000ff'",
        // And a move that changes nothing, so the refusal is unconditional
        // rather than predicate-driven.
        "seq = seq",
    ] {
        must_be_rejected(
            &conn,
            &format!(
                "UPDATE bss.pricing_audit_log SET {change} \
                 WHERE tenant_id = '{TENANT}' AND chain_id = '{CHAIN}'"
            ),
            "pricing_audit_log is append-only: UPDATE is not permitted",
        )
        .await;
    }
}

/// **Every** DELETE, on both kinds of row.
///
/// The roll-up case is not decoration: the trigger is `FOR EACH ROW` over the
/// whole table, and a later migration narrowing it with a `WHEN (OLD.entry_kind
/// = 'mutation')` — to let a retention sweep expire roll-ups, say — would leave
/// the mutation case green and open the row that ties the segments together.
///
/// No `REVOKE` backs this up anywhere in the chain, deliberately: it names a
/// deployment role the migration does not own and `SQLite` has no `GRANT` at
/// all. The trigger is the portable half, and it is the half that has to work.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_delete_is_refused_naming_the_operation() {
    let pg = applied().await;
    let conn = pg.raw().await;
    must_succeed(&conn, &insert(CHAIN, 0, "mutation", "NULL")).await;
    must_succeed(
        &conn,
        &insert(CHAIN, 1, "rollup", "'{\"heads\":[]}'::jsonb"),
    )
    .await;

    for seq in [0, 1] {
        must_be_rejected(
            &conn,
            &format!(
                "DELETE FROM bss.pricing_audit_log \
                 WHERE tenant_id = '{TENANT}' AND chain_id = '{CHAIN}' AND seq = {seq}"
            ),
            "pricing_audit_log is append-only: DELETE is not permitted",
        )
        .await;
    }

    assert_eq!(
        rows_in_segment(&conn, CHAIN).await,
        2,
        "and nothing was removed on the way to being refused"
    );
}

// ---------------------------------------------------------------------------
// The chain the writer builds
// ---------------------------------------------------------------------------

/// A vector instant carrying **sub-second** precision.
///
/// Deliberately not `hh:00:00`, for `sqlite_audit_chain.rs`'s reason: the digest
/// hashes `timestamp_micros()`, so an instant on the second would leave the one
/// column whose round trip can silently lose precision untested.
fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, 17, 42)
        .checked_add(time::Duration::microseconds(123_456))
        .expect("a fixed instant plus a fixed offset")
}

/// One record's worth of input, distinct in every field a later row could
/// confuse it with.
fn entry(chain_id: Uuid, subject: &str, hour: u32) -> NewAuditEntry {
    NewAuditEntry {
        tenant_id: TENANT,
        chain_id,
        recorded_at: at(hour),
        actor_principal_id: ACTOR,
        action: AuditAction::Publish,
        subject_kind: AuditSubjectKind::PlanRevision,
        subject_ref: subject.to_owned(),
        before_state: Some(json!({"lifecycleState": "draft", "rowVersion": 0})),
        after_state: Some(json!({"lifecycleState": "published", "rowVersion": 1})),
        approval_ref: Some(Uuid::from_u128(0xa9_90)),
        correlation_id: CORRELATION,
    }
}

/// One segment's rows, in chain order, read back through the scoped reader.
async fn segment(pg: &Pg, chain_id: Uuid) -> Vec<audit_log::Model> {
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let conn = provider.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(TENANT))
        .filter(
            Condition::all()
                .add(audit_log::Column::TenantId.eq(TENANT))
                .add(audit_log::Column::ChainId.eq(chain_id)),
        )
        .order_by(audit_log::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the segment")
}

/// Append `count` records to one segment, each through its own transaction, and
/// hand back the inputs the test supplied.
async fn append_segment(pg: &Pg, chain_id: Uuid, count: u32) -> Vec<NewAuditEntry> {
    let db = pg.db().await;
    let mut supplied = Vec::new();
    for revision in 0..count {
        let new = entry(chain_id, &format!("plan/{revision}"), 10 + revision);
        supplied.push(new.clone());
        let (_db, out) = db
            .clone()
            .in_transaction::<u64, RepoError, _>(move |txn| {
                Box::pin(async move {
                    audit_repo::append(txn, &AccessScope::for_tenant(TENANT), new).await
                })
            })
            .await;
        assert_eq!(
            out.expect("append"),
            u64::from(revision),
            "each append lands at the next position"
        );
    }
    supplied
}

/// **Connectedness, and nothing else.**
///
/// Deliberately says nothing about what any row contains: it asserts only that
/// the segment is a chain — dense from zero, seeded at genesis, each link
/// pointing at the previous row's digest. Letting one test carry both this and
/// the content check is how a suite ends up unable to tell a broken link from a
/// dropped field, because whichever assertion fires first hides the other.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_segments_links_connect_genesis_to_head() {
    let pg = applied().await;
    append_segment(&pg, CHAIN, 4).await;

    let rows = segment(&pg, CHAIN).await;
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows[0].prev_hash.as_deref(),
        Some(genesis_prev_hash(TENANT, CHAIN).as_slice()),
        "the first link is the segment's own seed, never NULL"
    );
    for (position, pair) in rows.windows(2).enumerate() {
        assert_eq!(
            pair[1].seq,
            pair[0].seq + 1,
            "a segment's positions are dense; a gap at {position} is a link nobody can walk"
        );
        assert_eq!(
            pair[1].prev_hash.as_deref(),
            Some(pair[0].row_hash.as_slice()),
            "row {} does not point at its predecessor's digest",
            position + 1
        );
    }
}

/// **Content, from an expectation this test built rather than one it read.**
///
/// The Phase-2 defect this is shaped against: a walk that rebuilds each preimage
/// *from the stored columns* is self-consistent by construction, so blanking
/// `before_state` and `after_state` on the writer leaves it green — the store
/// holds NULL, the recompute hashes NULL, and they agree. Here the expectation
/// comes from the [`NewAuditEntry`] values handed to `append`, so the two
/// assertions are independent of each other and of the row: the columns must
/// hold what was supplied, **and** the digest must be the hash of what was
/// supplied.
///
/// The running `prev_hash` is likewise the *computed* one, not the stored one —
/// a chain of expectations, so a single corrupted digest does not launder every
/// digest after it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_row_holds_the_record_its_writer_was_handed_and_hashes_to_it() {
    let pg = applied().await;
    let supplied = append_segment(&pg, CHAIN, 4).await;

    let rows = segment(&pg, CHAIN).await;
    assert_eq!(rows.len(), supplied.len());

    let mut expected_prev = genesis_prev_hash(TENANT, CHAIN);
    for (position, (row, input)) in rows.iter().zip(&supplied).enumerate() {
        let seq = u64::try_from(position).expect("a small segment");

        // The columns, against the input, not against each other.
        assert_eq!(row.tenant_id, input.tenant_id);
        assert_eq!(row.chain_id, input.chain_id);
        assert_eq!(u64::try_from(row.seq).expect("a position"), seq);
        assert_eq!(row.entry_kind, "mutation");
        assert_eq!(row.recorded_at, input.recorded_at);
        assert_eq!(row.actor_principal_id, input.actor_principal_id);
        assert_eq!(row.action, input.action.as_str());
        assert_eq!(row.subject_kind, input.subject_kind.as_str());
        assert_eq!(row.subject_ref, input.subject_ref);
        assert_eq!(
            row.before_state, input.before_state,
            "the before state the writer was handed must be the one the row holds"
        );
        assert_eq!(
            row.after_state, input.after_state,
            "and the after state with it: blanking either is the defect this asserts against"
        );
        assert_eq!(row.approval_ref, input.approval_ref);
        assert_eq!(row.correlation_id, Some(input.correlation_id));
        assert_eq!(row.segment_heads, None);

        // The digest, over the record this test constructed.
        let record = AuditRecord {
            tenant_id: input.tenant_id,
            chain_id: input.chain_id,
            seq,
            recorded_at: input.recorded_at,
            actor_principal_id: input.actor_principal_id,
            action: input.action,
            subject_kind: input.subject_kind,
            subject_ref: &input.subject_ref,
            before_state: input.before_state.as_ref(),
            after_state: input.after_state.as_ref(),
            approval_ref: input.approval_ref,
            correlation_id: Some(input.correlation_id),
        };
        let expected = audit_row_hash(&record, &expected_prev).expect("canonicalize");
        assert_eq!(
            row.row_hash.as_slice(),
            expected.as_slice(),
            "row {position} is not the digest of the record its writer was handed"
        );
        expected_prev = expected;
    }
}

// ---------------------------------------------------------------------------
// D-159 — two mutations of ONE aggregate, by execution
// ---------------------------------------------------------------------------

/// Block until the loser is provably waiting.
///
/// The observer is [`pg_support::wait_until_a_backend_blocks`], shared with the
/// other concurrency suite and narrowed to this test's own database — see its
/// doc for why the narrowing goes through `pg_stat_activity` and why it is
/// deliberately not narrowed by relation.
///
/// This is what makes the race a race rather than a coin toss: a backend in a
/// lock wait has already executed everything before the statement that blocked,
/// so observing it here proves the loser's head read happened **before** the
/// winner committed. Without it the loser could read a moved head and both
/// transactions would succeed.
async fn wait_until_a_backend_blocks(conn: &DatabaseConnection) {
    pg_support::wait_until_a_backend_blocks(conn).await;
}

/// The outcome of one choreographed same-segment race.
struct Race {
    /// The transaction that got the position.
    winner: Result<u64, TxError<RepoError>>,
    /// The transaction that did not — and what else it had already written.
    loser: Result<(u64, u64), TxError<RepoError>>,
    observer: DatabaseConnection,
    /// Kept alive for the length of the assertions: the database is this
    /// test's own, and the observer reads it.
    _pg: Pg,
}

/// Two transactions, one segment, one position — driven through
/// [`audit_repo::append`] on both sides.
///
/// The segment is seeded with a **committed** row first, so both racers read the
/// same non-empty head and both target `seq + 1`. That is D-159's literal shape;
/// racing on an empty segment would prove the same key but would not exercise
/// the head read the CONTRACT's argument is about.
///
/// The loser writes to a **second** segment before it touches the contended one.
/// That row is what makes "its whole transaction rolls back" checkable: it
/// succeeded on its own, it collided with nothing, and it must be gone.
async fn race_on_one_segment() -> Race {
    let pg = applied().await;
    let observer = pg.raw().await;

    let seed = pg.db().await;
    let (_seed, seeded) = seed
        .in_transaction::<u64, RepoError, _>(|txn| {
            Box::pin(async move {
                audit_repo::append(
                    txn,
                    &AccessScope::for_tenant(TENANT),
                    entry(CHAIN, "plan/seed", 9),
                )
                .await
            })
        })
        .await;
    assert_eq!(seeded.expect("seed the segment head"), 0);

    let inserted = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let winner = {
        let db = pg.db().await;
        let (inserted, release) = (Arc::clone(&inserted), Arc::clone(&release));
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<u64, RepoError, _>(move |txn| {
                    Box::pin(async move {
                        let seq = audit_repo::append(
                            txn,
                            &AccessScope::for_tenant(TENANT),
                            entry(CHAIN, "plan/winner", 10),
                        )
                        .await?;
                        // The row exists and is uncommitted. Park, so the loser
                        // reads a head that has not moved.
                        inserted.notify_one();
                        release.notified().await;
                        Ok(seq)
                    })
                })
                .await;
            out
        })
    };

    inserted.notified().await;

    let loser = {
        let db = pg.db().await;
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(u64, u64), RepoError, _>(|txn| {
                    Box::pin(async move {
                        let scope = AccessScope::for_tenant(TENANT);
                        // Uncontended, and therefore the witness for the
                        // rollback: nothing about this row is in dispute.
                        let bystander =
                            audit_repo::append(txn, &scope, entry(OTHER_CHAIN, "plan-b/0", 11))
                                .await?;
                        let contended =
                            audit_repo::append(txn, &scope, entry(CHAIN, "plan/loser", 12)).await?;
                        Ok((bystander, contended))
                    })
                })
                .await;
            out
        })
    };

    wait_until_a_backend_blocks(&observer).await;
    release.notify_one();

    let winner = tokio::time::timeout(RACE_TIMEOUT, winner)
        .await
        .expect("the winner must finish once released")
        .expect("the winner's task must not panic");
    let loser = tokio::time::timeout(RACE_TIMEOUT, loser)
        .await
        .expect("the loser must be released by the winner's commit")
        .expect("the loser's task must not panic");

    Race {
        winner,
        loser,
        observer,
        _pg: pg,
    }
}

/// D-159, end to end: the loser is told to retry, not that the store broke.
///
/// The whole ladder is asserted in one place because each step is where the
/// answer could go wrong: the repository recognises the driver's unique-violation
/// class rather than a message; `repo_failure` keeps it out of the `Internal`
/// arm; and the canonical ladder renders it **409 `CONCURRENT_MUTATION`**. A 500
/// here would tell a bulk run to page an operator about a race it should simply
/// re-drive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn the_loser_of_a_same_segment_race_is_a_retriable_contention() {
    let race = race_on_one_segment().await;

    assert_eq!(
        race.winner.expect("the winner must commit"),
        1,
        "the winner takes the position after the seeded head"
    );

    let refusal = match race.loser {
        Err(TxError::Domain(err)) => err,
        Ok(seqs) => panic!("both writers took one position: {seqs:?}"),
        Err(TxError::Infra(err)) => {
            panic!("a lost race must not reach the caller as an infrastructure fault: {err}")
        }
    };
    assert_eq!(
        refusal,
        RepoError::ConcurrentMutation {
            aggregate: format!("audit chain {CHAIN}"),
        },
        "the refusal must name the segment the caller retries against"
    );

    let domain = repo_failure(&refusal);
    assert!(
        matches!(domain, DomainError::ConcurrentMutation(_)),
        "not a contention at the domain boundary: {domain:?}"
    );

    let canonical = CanonicalError::from(domain);
    assert_eq!(canonical.status_code(), 409, "a race is not a server fault");
    assert!(
        format!("{canonical:?}").contains("CONCURRENT_MUTATION"),
        "the code is the discriminator, and it must survive the ladder: {canonical:?}"
    );
}

/// The other half of D-159, and the half a caller's data depends on: the loser's
/// **whole** transaction rolls back.
///
/// The witness is the row the loser wrote on an entirely different segment
/// before it collided. It contended with nothing and would have committed on its
/// own; if it survives, a failed mutation has left a partial audit trail behind
/// it — which is the one thing an evidence store may not do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn the_losers_whole_transaction_rolls_back() {
    let race = race_on_one_segment().await;
    race.winner.expect("the winner must commit");
    // **The variant, as the sibling case asserts it.** `expect_err` accepts any
    // error including `TxError::Infra` - the very arm the sibling panics on - so
    // it admits a loser that died of an infrastructure fault and rolled back for a
    // reason that has nothing to do with the race.
    assert!(
        matches!(
            race.loser,
            Err(TxError::Domain(RepoError::ConcurrentMutation { .. }))
        ),
        "the loser must lose the race, not fail some other way: {:?}",
        race.loser
    );

    assert_eq!(
        rows_in_segment(&race.observer, CHAIN).await,
        2,
        "the contended segment holds the seed and the winner, and nothing else"
    );
    assert_eq!(
        rows_in_segment(&race.observer, OTHER_CHAIN).await,
        0,
        "the loser's uncontended write went back with the rest of its transaction"
    );
}

// ---------------------------------------------------------------------------
// D-135 — two mutations of DIFFERENT aggregates, by execution
// ---------------------------------------------------------------------------

/// D-135's benefit, executed: two aggregates of one tenant do not share a head.
///
/// The first transaction appends to its segment and **stays open**. The second
/// then appends to a different segment and must run to completion *while the
/// first is still uncommitted* — enforced by [`RACE_TIMEOUT`], because a
/// mutation that contended would not fail, it would simply wait, and a test
/// without a clock would happily record that as success once the first side was
/// released.
///
/// The pairing with [`the_loser_of_a_same_segment_race_is_a_retriable_contention`]
/// is what makes this evidence rather than a tautology: that test shows the same
/// segment *does* block under the identical choreography, so finishing here is a
/// fact about the segmentation and not about the harness being too gentle to
/// provoke anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn two_aggregates_of_one_tenant_do_not_contend_on_a_chain_head() {
    let pg = applied().await;
    let observer = pg.raw().await;

    let inserted = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let first = {
        let db = pg.db().await;
        let (inserted, release) = (Arc::clone(&inserted), Arc::clone(&release));
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<u64, RepoError, _>(move |txn| {
                    Box::pin(async move {
                        let seq = audit_repo::append(
                            txn,
                            &AccessScope::for_tenant(TENANT),
                            entry(CHAIN, "plan-a/0", 10),
                        )
                        .await?;
                        inserted.notify_one();
                        release.notified().await;
                        Ok(seq)
                    })
                })
                .await;
            out
        })
    };

    inserted.notified().await;

    let second_db = pg.db().await;
    let second = tokio::spawn(async move {
        let (_db, out) = second_db
            .in_transaction::<u64, RepoError, _>(|txn| {
                Box::pin(async move {
                    audit_repo::append(
                        txn,
                        &AccessScope::for_tenant(TENANT),
                        entry(OTHER_CHAIN, "plan-b/0", 11),
                    )
                    .await
                })
            })
            .await;
        out
    });

    // **Sampled while the second mutation is still in flight**, which is the only
    // window in which a wait can be seen at all. This assertion used to be taken
    // after that mutation had already committed, at a point where the sole
    // remaining backend holds granted locks — so it could only ever read `0`, and
    // it was presented as the control distinguishing "did not contend" from
    // "waited its turn".
    //
    // Read after the loop rather than before the join, so that a contending
    // second answers *here* — a backend that is waiting on the first's lock stays
    // waiting until the release below, so the failing world produces a sharp
    // message instead of a thirty-second stall. The timeout on the join remains
    // the backstop for a wait this sampling missed: a second that commits before
    // the first sample lands leaves the loop with nothing measured, which is
    // honest — the sample can only ever *find* a wait, never prove there was
    // none.
    let mut ever_blocked = 0_i64;
    loop {
        ever_blocked = ever_blocked.max(pg_support::blocked_backends(&observer).await);
        if ever_blocked > 0 || second.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ever_blocked, 0,
        "a backend waited on a lock while the second aggregate's mutation was in flight: \
         the chain is NOT segmented per aggregate and D-135's performance argument does \
         not hold"
    );

    let second = tokio::time::timeout(RACE_TIMEOUT, second)
        .await
        .expect(
            "a mutation of a second aggregate did not finish while the first aggregate's \
             transaction was open: the chain is NOT segmented per aggregate and D-135's \
             performance argument does not hold",
        )
        .expect("the second aggregate's task must not panic");
    assert_eq!(
        second.expect("the second aggregate's mutation must commit"),
        0,
        "each segment starts at its own genesis"
    );

    release.notify_one();
    let first = tokio::time::timeout(RACE_TIMEOUT, first)
        .await
        .expect("the first transaction must finish once released")
        .expect("the first task must not panic")
        .expect("the first aggregate's mutation must commit");
    assert_eq!(first, 0);

    assert_eq!(rows_in_segment(&observer, CHAIN).await, 1);
    assert_eq!(rows_in_segment(&observer, OTHER_CHAIN).await, 1);
}
