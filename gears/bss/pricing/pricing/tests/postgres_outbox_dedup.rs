//! `pricing_outbox`'s deduplication refusal, measured on the engine that serves
//! it.
//!
//! # Why this belongs in the Postgres tier and not beside its `SQLite` twin
//!
//! A repeated announcement violates **two** constraints at one insert.
//! `outbox_id` is a `v5` digest of `(tenant_id, dedup_key)` and it is the table's
//! primary key, so `pricing_outbox_pkey` and `uq_pricing_outbox_dedup_key` have
//! the same operand and no input trips one without the other. Which of the two a
//! driver names is the engine's choice: Postgres checks in index OID order and
//! the primary key is created with the table, `SQLite` names the columns of
//! whichever it evaluated.
//!
//! `outbox_repo`'s recognizer decides the refusal class off that name, so the
//! `SQLite` cases in `sqlite_publish_commit` and `sqlite_bundle_publish` prove
//! the arm on one engine only — and the arm exists precisely to stop answering
//! `CONCURRENT_MUTATION`, whose remedy is a retry that rebuilds the same key and
//! collides identically. An engine that names the other index answers the exact
//! advice the arm removes, with every `SQLite` case green.

mod pg_support;

use bss_pricing::domain::events::CatalogEvent;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::repo::{NewOutboxEvent, outbox_repo};

use bss_pricing::domain::instant::utc_ymd_hms;
use pg_support::Pg;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_42);
const AGGREGATE: Uuid = Uuid::from_u128(0xa6_40);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_44);

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 9, hour, 0, 0)
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

fn event(dedup_key: &str) -> NewOutboxEvent {
    NewOutboxEvent {
        tenant_id: TENANT,
        aggregate_id: AGGREGATE,
        event: CatalogEvent::PlanPublished,
        payload: serde_json::json!({ "planId": AGGREGATE.to_string() }),
        dedup_key: dedup_key.to_owned(),
        correlation_id: CORRELATION,
        enqueued_at: at(10),
    }
}

/// The second announcement of one act is refused as **already enqueued**, not as
/// contention.
///
/// The distinction is the whole of the arm: contention is a race a retry
/// resolves, and this is deterministic — the retry renders the same key from the
/// same inputs and lands on the same two indexes.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_repeated_announcement_is_already_enqueued_rather_than_contention() {
    let pg = Pg::applied().await;
    let db = DBProvider::<DbError>::new(pg.db().await);
    let conn = db.conn().expect("conn");

    let seq = outbox_repo::enqueue(&conn, &scope(), event("plan-published/1/0"))
        .await
        .expect("the first announcement lands");
    assert_eq!(seq, 0, "the aggregate's first event opens its sequence");

    let refusal = outbox_repo::enqueue(&conn, &scope(), event("plan-published/1/0"))
        .await
        .expect_err("the same act must not be announced twice");

    assert!(
        matches!(
            refusal,
            RepoError::OutboxEventAlreadyEnqueued { ref dedup_key }
                if dedup_key == "plan-published/1/0"
        ),
        "Postgres names one of the two indexes the repeat violates, and the class \
         must not depend on which: {refusal:?}"
    );
}

/// A **different** act on the same aggregate still enqueues, so the case above is
/// about the key and not about the second insert.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_second_distinct_act_on_one_aggregate_still_enqueues() {
    let pg = Pg::applied().await;
    let db = DBProvider::<DbError>::new(pg.db().await);
    let conn = db.conn().expect("conn");

    outbox_repo::enqueue(&conn, &scope(), event("plan-published/2/0"))
        .await
        .expect("the first");
    let seq = outbox_repo::enqueue(&conn, &scope(), event("plan-published/2/1"))
        .await
        .expect("a distinct act is a distinct event");

    assert_eq!(seq, 1, "and it takes the aggregate's next position");
}
