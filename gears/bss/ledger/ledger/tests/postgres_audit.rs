//! Postgres-only end-to-end for the secured audit store (Slice 6 Phase 2 Group
//! 2A). Boots a container and migrates, then drives `SecuredAuditStore::append`
//! inside a transaction and asserts the sealed-record + chain invariants: a
//! first append is sealed (row_hash/prev_hash non-NULL) with the tenant genesis
//! prev_hash and writes the tip; a second append links onto the first
//! (`prev_hash == first.row_hash`); a raw UPDATE/DELETE on the record is
//! rejected (append-only); a clean re-walk recomputes the seal exactly, and a
//! tampered row (trigger disabled + UPDATE) breaks the recompute.
//! Ignored by default; run with `cargo test -p cf-gears-bss-ledger -- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::integer_division,
    clippy::panic
)]

use bss_ledger::domain::audit_chain::{AuditHashInput, audit_genesis_prev_hash, audit_row_hash};
use bss_ledger::infra::audit::event_type::AuditEventType;
use bss_ledger::infra::audit::store::SecuredAuditStore;
use bss_ledger::infra::storage::migrations::Migrator;

use bss_ledger::domain::instant::from_unix_millis;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, TxConfig};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// Hex string of a single-column, single-row SELECT, `None` when absent/NULL.
async fn scalar_hex(conn: &DatabaseConnection, sql: &str) -> Option<String> {
    let row = conn.query_one_raw(pg(sql.to_owned())).await.unwrap();
    row.and_then(|r| r.try_get_by_index::<Option<String>>(0).unwrap())
}

/// Hex string of a 32-byte hash for embedding in a Postgres text comparison.
fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Boot, migrate, return the migrate connection + the bss-search-path provider.
async fn setup(container_url: &str) -> (DatabaseConnection, DBProvider<DbError>) {
    let raw = Database::connect(container_url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{container_url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    (raw, DBProvider::<DbError>::new(tdb))
}

/// Append one record in its own transaction, returning the generated audit id.
#[allow(clippy::too_many_arguments)]
async fn append(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    event_type: AuditEventType,
    actor_ref: Option<String>,
    reason_code: Option<String>,
    before_after: serde_json::Value,
    correlation_id: Option<Uuid>,
    retain_until: Option<OffsetDateTime>,
) -> Uuid {
    let scope = AccessScope::for_tenant(tenant);
    provider
        .transaction(move |txn| {
            Box::pin(async move {
                SecuredAuditStore::new()
                    .append(
                        txn,
                        &scope,
                        tenant,
                        event_type,
                        actor_ref.as_deref(),
                        reason_code.as_deref(),
                        &before_after,
                        correlation_id,
                        retain_until,
                    )
                    .await
            })
        })
        .await
        .expect("append must succeed")
}

/// A first append is sealed (row_hash/prev_hash non-NULL), its prev_hash is the
/// tenant genesis seed, and the tip points at it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn appends_first_record_sealed_with_genesis_prev() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider) = setup(&url).await;
    let tenant = Uuid::now_v7();

    let audit_id = append(
        &provider,
        tenant,
        AuditEventType::MetadataChange,
        Some("actor-1".to_owned()),
        Some("rc-1".to_owned()),
        json!({"before": 1, "after": 2}),
        Some(Uuid::now_v7()),
        None,
    )
    .await;

    let row_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(row_hash,'hex') FROM bss.secured_audit_record WHERE audit_id='{audit_id}'"
        ),
    )
    .await;
    assert!(row_hash.is_some(), "row_hash must be sealed (non-NULL)");
    let row_hash = row_hash.unwrap();

    let prev_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(prev_hash,'hex') FROM bss.secured_audit_record WHERE audit_id='{audit_id}'"
        ),
    )
    .await
    .expect("prev_hash must be set");
    assert_eq!(
        prev_hash,
        hex32(&audit_genesis_prev_hash(tenant)),
        "first record prev_hash must be the tenant genesis seed"
    );

    let tip_row_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(last_row_hash,'hex') FROM bss.audit_chain_state WHERE tenant_id='{tenant}'"
        ),
    )
    .await
    .expect("audit_chain_state tip must exist");
    assert_eq!(
        tip_row_hash, row_hash,
        "tip last_row_hash must equal row_hash"
    );

    let tip_points: i64 = raw
        .query_one_raw(pg(format!(
            "SELECT COUNT(*) FROM bss.audit_chain_state \
             WHERE tenant_id='{tenant}' AND last_audit_id='{audit_id}' AND last_seq=1"
        )))
        .await
        .unwrap()
        .map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap());
    assert_eq!(
        tip_points, 1,
        "tip must reference the sealed audit id at seq 1"
    );
}

/// A second append links onto the first: its prev_hash equals the first's
/// row_hash, and the tip advances to seq 2.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn links_second_record_to_first() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider) = setup(&url).await;
    let tenant = Uuid::now_v7();

    let first = append(
        &provider,
        tenant,
        AuditEventType::ConfigChange,
        None,
        None,
        json!({}),
        None,
        None,
    )
    .await;
    let second = append(
        &provider,
        tenant,
        AuditEventType::Erasure,
        Some("actor-2".to_owned()),
        None,
        json!({"x": 1}),
        None,
        None,
    )
    .await;

    let first_row_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(row_hash,'hex') FROM bss.secured_audit_record WHERE audit_id='{first}'"
        ),
    )
    .await
    .expect("first row_hash");
    let second_prev_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(prev_hash,'hex') FROM bss.secured_audit_record WHERE audit_id='{second}'"
        ),
    )
    .await
    .expect("second prev_hash");
    assert_eq!(
        second_prev_hash, first_row_hash,
        "second record prev_hash must equal first record row_hash"
    );

    let tip_seq: i64 = raw
        .query_one_raw(pg(format!(
            "SELECT last_seq FROM bss.audit_chain_state WHERE tenant_id='{tenant}'"
        )))
        .await
        .unwrap()
        .map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap());
    assert_eq!(tip_seq, 2, "tip must advance to seq 2");
}

/// The append-only trigger rejects a raw UPDATE and DELETE on a sealed record.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn append_only_trigger_rejects_update_and_delete() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider) = setup(&url).await;
    let tenant = Uuid::now_v7();

    let audit_id = append(
        &provider,
        tenant,
        AuditEventType::FreezeSetClear,
        None,
        None,
        json!({}),
        None,
        None,
    )
    .await;

    let upd = raw
        .execute_raw(pg(format!(
            "UPDATE bss.secured_audit_record SET reason_code='X' WHERE audit_id='{audit_id}'"
        )))
        .await;
    let upd_err = upd.expect_err("an UPDATE must be rejected").to_string();
    assert!(
        upd_err.contains("append-only"),
        "UPDATE error must mention append-only, got: {upd_err}"
    );

    let del = raw
        .execute_raw(pg(format!(
            "DELETE FROM bss.secured_audit_record WHERE audit_id='{audit_id}'"
        )))
        .await;
    let del_err = del.expect_err("a DELETE must be rejected").to_string();
    assert!(
        del_err.contains("append-only"),
        "DELETE error must mention append-only, got: {del_err}"
    );
}

/// A clean re-walk recomputes each record's row_hash exactly; a tampered row
/// (trigger disabled + row_hash overwritten) is **reported by a re-walk run over
/// the row as it now stands**, at the row itself and at its successor's
/// back-link.
///
/// # What this does and does not exercise
///
/// The re-walk is this test's own, over
/// [`bss_ledger::domain::audit_chain::audit_row_hash`] — the encoder the seal
/// uses — because **nothing in production re-walks this chain**.
/// `ChainVerifierJob` walks `ledger_journal_entry` via `chain_row_hash` and knows
/// nothing about `secured_audit_record`, and `audit_row_hash` has exactly one
/// production caller: the seal in `SecuredAuditStore::append`. So this asserts
/// that the chain *carries* the evidence of a tamper, not that any deployed code
/// acts on it; the sibling `postgres_chain.rs::verifier_freezes_tampered_chain`
/// is the journal chain's version of the latter and the audit chain has no
/// counterpart.
///
/// The tamper half used to end at `assert_ne!(recomputed, tampered_hash)` against
/// the `deadbeef…` constant the test had just written, which said only that the
/// test's own `UPDATE` had landed. What is asserted now is the pair of checks a
/// walker makes — content-vs-seal on the row, and seal-vs-`prev_hash` across the
/// link — and the second of them is what localises the break to the tampered row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn rewalk_detects_tamper() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider) = setup(&url).await;
    let tenant = Uuid::now_v7();

    // A sealed 2-link chain.
    let first = append(
        &provider,
        tenant,
        AuditEventType::MetadataChange,
        Some("a-1".to_owned()),
        None,
        json!({"k": 1}),
        None,
        None,
    )
    .await;
    let second = append(
        &provider,
        tenant,
        AuditEventType::MetadataChange,
        Some("a-2".to_owned()),
        None,
        json!({"k": 2}),
        None,
        None,
    )
    .await;

    // Clean re-walk: recompute the FIRST record's row_hash from its stored
    // columns (genesis prev) and assert it matches the sealed value.
    let clean = rewalk_record(&raw, tenant, first, &audit_genesis_prev_hash(tenant)).await;
    assert_eq!(
        clean.recomputed, clean.stored,
        "clean re-walk must recompute the sealed row_hash exactly"
    );
    // The link the walk crosses next, while the chain is still whole.
    assert_eq!(
        prev_hash_of(&raw, second).await,
        clean.stored,
        "the successor's prev_hash is its parent's sealed row_hash"
    );

    // Tamper the first record's row_hash out-of-band (the append-only trigger
    // forbids any UPDATE, so disable it for the tamper and re-enable after).
    raw.execute_raw(pg(
        "ALTER TABLE bss.secured_audit_record DISABLE TRIGGER trg_secured_audit_append_only",
    ))
    .await
    .unwrap();
    raw.execute_raw(pg(format!(
        "UPDATE bss.secured_audit_record \
         SET row_hash = decode('deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef','hex') \
         WHERE audit_id='{first}'"
    )))
    .await
    .unwrap();
    raw.execute_raw(pg(
        "ALTER TABLE bss.secured_audit_record ENABLE TRIGGER trg_secured_audit_append_only",
    ))
    .await
    .unwrap();

    // The re-walk is run AGAIN, over the row as it now stands. Nothing about the
    // record's *content* moved, so the recompute is unchanged and the stored seal
    // is not: that disagreement is the break, and it is read out of the database
    // rather than compared against the constant this test wrote.
    let broken = rewalk_record(&raw, tenant, first, &audit_genesis_prev_hash(tenant)).await;
    assert_eq!(
        broken.recomputed, clean.recomputed,
        "the tamper moved the seal, not the content it seals"
    );
    assert_ne!(
        broken.recomputed, broken.stored,
        "the re-walk must report the break: the stored row_hash no longer matches what \
         this record's own columns hash to"
    );

    // And the back-link is broken too, which is how a walk localises the break to
    // this row rather than to its successor: the successor still carries the
    // parent's ORIGINAL seal.
    let successor_prev = prev_hash_of(&raw, second).await;
    assert_ne!(
        successor_prev, broken.stored,
        "the successor's prev_hash must no longer equal its parent's stored row_hash"
    );
    assert_eq!(
        successor_prev, broken.recomputed,
        "and it still equals what the parent's content hashes to, so the break is the \
         parent's seal and not the child's link"
    );
}

/// One record's seal, as stored and as recomputed from its own stored columns.
struct Rewalked {
    /// `row_hash` recomputed over [`audit_row_hash`] — the encoder the seal used.
    recomputed: String,
    /// `row_hash` as the row currently carries it.
    stored: String,
}

/// Re-walk one record: read its stored columns, recompute the seal over `prev`,
/// and answer both hashes as hex.
///
/// Read back out of the database on **every** call, so that running it after a
/// tamper measures the tampered row rather than restating an earlier result.
async fn rewalk_record(
    raw: &DatabaseConnection,
    tenant: Uuid,
    audit_id: Uuid,
    prev: &[u8; 32],
) -> Rewalked {
    // Microseconds since the Unix epoch. For current-era timestamps the value
    // (~1.8e15) is well within f64's exact-integer range (< 2^53), so the
    // epoch-seconds product and the bigint cast are lossless — the recompute
    // matches the seal exactly.
    let row = raw
        .query_one_raw(pg(format!(
            "SELECT event_type, actor_ref, reason_code, before_after::text, \
                    correlation_id::text, (EXTRACT(epoch FROM at_utc) * 1000000)::bigint, \
                    encode(row_hash,'hex') \
             FROM bss.secured_audit_record WHERE audit_id='{audit_id}'"
        )))
        .await
        .unwrap()
        .expect("the record row");
    let event_type: String = row.try_get_by_index(0).unwrap();
    let actor_ref: Option<String> = row.try_get_by_index(1).unwrap();
    let reason_code: Option<String> = row.try_get_by_index(2).unwrap();
    let before_after_text: String = row.try_get_by_index(3).unwrap();
    let correlation_text: Option<String> = row.try_get_by_index(4).unwrap();
    let at_micros: i64 = row.try_get_by_index(5).unwrap();
    let stored: String = row.try_get_by_index(6).unwrap();

    let before_after: serde_json::Value = serde_json::from_str(&before_after_text).unwrap();
    let correlation_id = correlation_text.map(|s| Uuid::parse_str(&s).unwrap());
    let at_utc = from_unix_millis((at_micros) / 1000).unwrap();

    let recomputed = audit_row_hash(
        &AuditHashInput {
            audit_id,
            tenant_id: tenant,
            event_type: &event_type,
            actor_ref: actor_ref.as_deref(),
            reason_code: reason_code.as_deref(),
            correlation_id,
            at_utc,
            before_after: &before_after,
        },
        prev,
    )
    .expect("recompute audit row_hash");
    Rewalked {
        recomputed: hex32(&recomputed),
        stored,
    }
}

/// One record's stored `prev_hash`, as hex.
async fn prev_hash_of(raw: &DatabaseConnection, audit_id: Uuid) -> String {
    scalar_hex(
        raw,
        &format!(
            "SELECT encode(prev_hash,'hex') FROM bss.secured_audit_record \
             WHERE audit_id='{audit_id}'"
        ),
    )
    .await
    .expect("the record's prev_hash")
}

/// Collect a single text column over many rows into a `Vec<String>`.
async fn fetch_hex_set(conn: &DatabaseConnection, sql: &str) -> Vec<String> {
    conn.query_all_raw(pg(sql.to_owned()))
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.try_get_by_index::<String>(0).unwrap())
        .collect()
}

/// Z4-5b: concurrent appends to one tenant's audit chain form a SINGLE linear
/// chain (no fork) — the same SSI invariant the journal chain relies on
/// (mirrors `postgres_chain.rs::concurrent_posts_form_linear_chain`). The audit
/// `append` does a lockless tip read-then-advance, so without SERIALIZABLE two
/// racing appends could seal onto the same prev_hash and fork; here every
/// committed record links onto a distinct parent.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_appends_form_linear_audit_chain() {
    const N: usize = 8;
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider) = setup(&url).await;
    let tenant = Uuid::now_v7();

    // Race N appends onto the per-tenant audit tip. A SERIALIZABLE loser aborts
    // (SSI 40001, retryable) and retries from a fresh tip — a bounded manual loop
    // here stands in for the posting service's `transaction_with_retry`.
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let p = provider.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..50 {
                let scope = AccessScope::for_tenant(tenant);
                let ba = json!({ "i": i });
                let res: Result<Uuid, DbError> = p
                    .transaction_with_config(TxConfig::serializable(), move |txn| {
                        Box::pin(async move {
                            SecuredAuditStore::new()
                                .append(
                                    txn,
                                    &scope,
                                    tenant,
                                    AuditEventType::ConfigChange,
                                    None,
                                    None,
                                    &ba,
                                    None,
                                    None,
                                )
                                .await
                        })
                    })
                    .await;
                if res.is_ok() {
                    return;
                }
            }
            panic!("append {i} did not converge under contention");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // Every append is sealed and the row_hashes are all distinct.
    let row_hashes = fetch_hex_set(
        &raw,
        &format!(
            "SELECT encode(row_hash,'hex') FROM bss.secured_audit_record \
             WHERE tenant_id='{tenant}' ORDER BY at_utc"
        ),
    )
    .await;
    assert_eq!(row_hashes.len(), N, "every append must be sealed");
    let mut distinct = row_hashes.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), N, "all row_hash must be distinct");

    // No two records share a prev_hash (a fork would re-use a prev link); exactly
    // one links to genesis, and every other prev_hash matches some row_hash.
    let prev_hashes = fetch_hex_set(
        &raw,
        &format!(
            "SELECT encode(prev_hash,'hex') FROM bss.secured_audit_record \
             WHERE tenant_id='{tenant}'"
        ),
    )
    .await;
    let mut distinct_prev = prev_hashes.clone();
    distinct_prev.sort();
    distinct_prev.dedup();
    assert_eq!(
        distinct_prev.len(),
        N,
        "no two records may share a prev_hash (single linear chain)"
    );
    let genesis = hex32(&audit_genesis_prev_hash(tenant));
    assert_eq!(
        prev_hashes.iter().filter(|h| **h == genesis).count(),
        1,
        "exactly one record links to the genesis seed"
    );
    for prev in &prev_hashes {
        if *prev == genesis {
            continue;
        }
        assert!(
            row_hashes.contains(prev),
            "every non-genesis prev_hash must match another record's row_hash"
        );
    }

    // The tip advanced exactly N times.
    let tip_seq: i64 = raw
        .query_one_raw(pg(format!(
            "SELECT last_seq FROM bss.audit_chain_state WHERE tenant_id='{tenant}'"
        )))
        .await
        .unwrap()
        .map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap());
    assert_eq!(
        tip_seq,
        i64::try_from(N).unwrap(),
        "the audit tip must advance once per committed append"
    );
}
