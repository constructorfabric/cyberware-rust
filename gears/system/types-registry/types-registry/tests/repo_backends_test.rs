//! The repository primitives (T4) on `PostgreSQL` and `MySQL`.
//!
//! `repo_test.rs` covers `SQLite` unconditionally. This file is not a duplicate
//! suite — it holds the four properties `SQLite` cannot demonstrate:
//!
//! * **`create_or_get` reads the loser's unique violation as the serialization
//!   point.** `is_unique_violation()` classifies a driver-specific error code
//!   (`23505` / `1062` / `SQLITE_CONSTRAINT_UNIQUE`), and a misclassification on one
//!   backend turns an ordinary race into a failed admission.
//! * **The current-document read binds a disjunction of exact
//!   `(entity_id, revision_no)` pairs** into a backend-specific large-text column
//!   (`text` / `LONGTEXT`). `SQLite` is typeless and has no parameter shape to get
//!   wrong, so it can vouch for neither.
//! * **A multi-statement read needs snapshot isolation, not merely a transaction.**
//!   `PostgreSQL` defaults to `READ COMMITTED`, where two reads in one transaction
//!   can straddle a concurrent commit; `ports::snapshot_read()` asks for
//!   `RepeatableRead` + `ReadOnly` to close that. `SQLite` maps every level onto
//!   `Serializable` and cannot tell the two apart.
//! * **The keyset cursor's total order is the column's collation.** `COLLATE "C"` /
//!   `ascii_bin` is what makes `ORDER BY gts_id` and `gts_id > :after` byte order;
//!   under a locale collation the cursor's order would stop matching the order the
//!   caller pages in (`constraint-multi-backend`). `SQLite` compares `TEXT` bytewise
//!   with no collation to get wrong.
//!
//! Gated behind `--features integration` because it needs a Docker daemon:
//!
//! ```text
//! cargo test -p cf-gears-types-registry --features integration --test repo_backends_test
//! ```

#![cfg(feature = "integration")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use gts::GtsIdPattern;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::secure::SecureUpdateExt;
use toolkit_db::{DBProvider, DbError};
use toolkit_gts::gts_id;
use uuid::Uuid;

use common::{
    allow_all, provider_for, seed_current_type_schema, seed_operation_item,
    seed_pending_revision_item, seed_type_schema_revision,
};
use types_registry::domain::enums::{DependencyKind, EntityKind, OwnershipScope};
use types_registry::domain::ports::{
    CurrentSchemaCas, NewCurrentTypeSchema, NewEntity, ReverseImpact, commit_write, snapshot_read,
};
use types_registry::infra::storage::entity::type_schema;
use types_registry::infra::storage::repo::{
    DependencyRepo, EntityRepo, OperationRepo, PageRequest, TypeSchemaRepo, VersionFamilyRepo,
};

const NOW: OffsetDateTime = datetime!(2026-08-18 09:15:30 UTC);
const FAMILY_KEY: &str = "gts.acme.crm.customer.type";

type Provider = Arc<DBProvider<DbError>>;

/// The family and member the two in-transaction race tests contend over. Separate
/// from [`FAMILY_KEY`] so the races start from an empty key, and inserted **last** in
/// the suite because [`keyset_pages_in_byte_order`] and `pattern_list_agrees_with_gts`
/// enumerate every entity row.
const TX_FAMILY: &str = "gts.acme.crm.order.type";
const TX_RACED_ID: &str = gts_id!("acme.crm.order.type.v1~");

/// Identifiers whose byte order differs from a locale collation's order.
///
/// `_` is `0x5F` and `b` is `0x62`, so bytewise `a_b` sorts before `ab`. A glibc
/// locale collation weighs punctuation only as a tiebreak and orders them the
/// other way round, so the order this array is written in is evidence about the
/// column's collation rather than an arbitrary choice. Every identifier is
/// in-grammar: `_` is a legal `namespace` character.
const BYTE_ORDERED: &[&str] = &[
    gts_id!("acme.crm.a_b.type.v1~"),
    gts_id!("acme.crm.ab.type.v1~"),
    gts_id!("acme.crm.a_c.type.v1~"),
    gts_id!("acme.crm.ac.type.v1~"),
];

async fn wait_for_tcp(host: &str, port: u16, timeout: Duration) {
    use tokio::net::TcpStream;
    use tokio::time::{Instant, sleep};
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect((host, port)).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timeout waiting for {host}:{port}"
        );
        sleep(Duration::from_millis(200)).await;
    }
}

fn new_entity(gts_id: &str, family_id: i64) -> NewEntity {
    NewEntity {
        gts_uuid: Uuid::new_v5(&Uuid::NAMESPACE_URL, gts_id.as_bytes()),
        gts_id: gts_id.to_owned(),
        entity_kind: EntityKind::TypeSchema,
        family_id,
        ownership_scope: OwnershipScope::Global,
        owner_tenant_id: None,
        owning_gear: Some("types-registry".to_owned()),
        now: NOW,
    }
}

/// Concurrent first admission of one family: whatever the interleaving, exactly
/// one caller creates the row and every caller comes back with the same one.
///
/// PostgreSQL and `MySQL` have real row-level concurrency, so unlike the
/// `SQLite` version of this test there is no busy-lock retry here — a losing
/// caller must be served by `is_unique_violation()` and the re-read, not by
/// waiting.
async fn family_race_yields_one_row(db: &Provider, backend: &str) {
    let mut handles = Vec::new();
    for _ in 0..8 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            let conn = db.conn().expect("conn");
            VersionFamilyRepo::create_or_get(
                &conn,
                &allow_all(),
                FAMILY_KEY,
                OwnershipScope::Global,
                None,
                NOW,
            )
            .await
            .expect("create_or_get must absorb the loser's unique violation")
        }));
    }

    let mut ids = Vec::new();
    let mut creators = 0;
    for handle in handles {
        let (model, created) = handle.await.expect("task");
        ids.push(model.id);
        if created {
            creators += 1;
        }
    }
    assert_eq!(
        creators, 1,
        "exactly one caller may create the family on {backend}"
    );
    assert!(
        ids.windows(2).all(|w| w[0] == w[1]),
        "every caller must agree on the family row on {backend}: {ids:?}"
    );
}

/// The keyset cursor pages in the column's order, and that order must be byte
/// order — see [`BYTE_ORDERED`].
async fn keyset_pages_in_byte_order(db: &Provider, family_id: i64, backend: &str) {
    let conn = db.conn().expect("conn");
    for id in BYTE_ORDERED {
        EntityRepo::insert(&conn, &allow_all(), new_entity(id, family_id))
            .await
            .unwrap_or_else(|e| panic!("insert {id} on {backend}: {e}"));
    }

    let mut seen: Vec<String> = Vec::new();
    let mut request = PageRequest::first(2);
    loop {
        let page = EntityRepo::list_page(&conn, &allow_all(), None, request)
            .await
            .expect("page");
        seen.extend(page.items.iter().map(|m| m.gts_id.clone()));
        if !page.has_more {
            break;
        }
        request = PageRequest::after(page.next_after.expect("cursor when more remains"), 2);
    }

    let mut expected: Vec<String> = BYTE_ORDERED.iter().map(|s| (*s).to_owned()).collect();
    expected.sort();
    assert_eq!(
        seen, expected,
        "the keyset cursor must page in byte order on {backend}; a locale \
         collation would order the `_`-bearing identifiers differently"
    );
}

/// The prefix range narrows in SQL and `GtsId::matches_pattern` decides, on every
/// backend. `ab` shares the range with `a_b` but not the pattern.
async fn pattern_list_agrees_with_gts(db: &Provider, backend: &str) {
    let conn = db.conn().expect("conn");
    let pattern = GtsIdPattern::try_new(gts_id!("acme.crm.a_b.type.v1~")).expect("pattern");
    let page = EntityRepo::list_page(&conn, &allow_all(), Some(&pattern), PageRequest::first(10))
        .await
        .expect("list");
    let ids: Vec<&str> = page.items.iter().map(|m| m.gts_id.as_str()).collect();
    assert_eq!(
        ids,
        vec![gts_id!("acme.crm.a_b.type.v1~")],
        "only matches_pattern may decide, on {backend}"
    );
}

/// Compare-and-swap on `resource_version`: the affected-row count is the success
/// signal, and a stale precondition is a reported outcome rather than an error.
///
/// Worth asserting per backend because `MySQL` reports *changed* rows rather
/// than *matched* rows depending on the client flags, and a driver that reported
/// matched rows would make a no-op update look like a success.
async fn cas_reports_by_affected_rows(db: &Provider, family_id: i64, backend: &str) {
    let conn = db.conn().expect("conn");
    let scope = allow_all();
    let gts_id = gts_id!("acme.crm.cas.type.v1~");
    let row = EntityRepo::insert(&conn, &scope, new_entity(gts_id, family_id))
        .await
        .expect("insert")
        .expect("the identifier is free");
    assert_eq!(row.resource_version, 1);

    assert_eq!(
        EntityRepo::compare_and_swap_version(&conn, &scope, row.id, 1, NOW)
            .await
            .expect("cas"),
        Some(2),
        "a successful CAS returns the value it wrote on {backend}"
    );
    assert_eq!(
        EntityRepo::compare_and_swap_version(&conn, &scope, row.id, 1, NOW)
            .await
            .expect("a stale CAS reports failure rather than erroring"),
        None,
        "a stale expected version must affect zero rows on {backend}"
    );
    let reread = EntityRepo::find_by_gts_id(&conn, &scope, gts_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        reread.resource_version, 2,
        "the failed CAS changed nothing on {backend}"
    );
}

/// The closure walk over real foreign keys: `fk_tr_dependency_*` are declared
/// `RESTRICT` on PostgreSQL and `MySQL` but not enforced by default on `SQLite`,
/// so an edge insert that named a non-existent endpoint would only fail here.
async fn closure_walks_a_chain(db: &Provider, family_id: i64, backend: &str) {
    let conn = db.conn().expect("conn");
    let scope = allow_all();
    let chain = [
        gts_id!("acme.crm.chain_a.type.v1~"),
        gts_id!("acme.crm.chain_b.type.v1~"),
        gts_id!("acme.crm.chain_c.type.v1~"),
    ];
    let mut ids = Vec::new();
    for id in chain {
        ids.push(
            EntityRepo::insert(&conn, &scope, new_entity(id, family_id))
                .await
                .expect("insert chain member")
                .expect("the identifier is free")
                .id,
        );
    }
    DependencyRepo::replace_outgoing(
        &conn,
        &scope,
        ids[0],
        &[(DependencyKind::SchemaRef, ids[1])],
    )
    .await
    .expect("a -> b");
    DependencyRepo::replace_outgoing(
        &conn,
        &scope,
        ids[1],
        &[(DependencyKind::SchemaRef, ids[2])],
    )
    .await
    .expect("b -> c");

    let closure = DependencyRepo::closure(&conn, &scope, &[chain[0].to_owned()])
        .await
        .expect("closure");
    let got: Vec<&str> = closure.entities.iter().map(|m| m.gts_id.as_str()).collect();
    let mut expected: Vec<&str> = chain.to_vec();
    expected.sort_unstable();
    assert_eq!(
        got, expected,
        "the whole chain and nothing else, gts_id-sorted, on {backend}"
    );
    assert!(closure.missing_roots.is_empty());
}

async fn reverse_impact_walks_back_up_a_chain(db: &Provider, family_id: i64, backend: &str) {
    let conn = db.conn().expect("conn");
    let scope = allow_all();
    let chain = [
        gts_id!("acme.crm.rev_a.type.v1~"),
        gts_id!("acme.crm.rev_b.type.v1~"),
        gts_id!("acme.crm.rev_c.type.v1~"),
    ];
    let mut ids = Vec::new();
    for id in chain {
        ids.push(
            EntityRepo::insert(&conn, &scope, new_entity(id, family_id))
                .await
                .expect("insert chain member")
                .expect("the identifier is free")
                .id,
        );
    }
    // a -> b -> c, so the reverse impact of c is b and a.
    DependencyRepo::replace_outgoing(
        &conn,
        &scope,
        ids[0],
        &[(DependencyKind::SchemaRef, ids[1])],
    )
    .await
    .expect("a -> b");
    DependencyRepo::replace_outgoing(
        &conn,
        &scope,
        ids[1],
        &[(DependencyKind::SchemaRef, ids[2])],
    )
    .await
    .expect("b -> c");
    DependencyRepo::replace_outgoing(
        &conn,
        &scope,
        ids[2],
        &[(DependencyKind::SchemaRef, ids[0])],
    )
    .await
    .expect("c -> a");

    match DependencyRepo::reverse_impact(&conn, &scope, &[ids[2]], 512)
        .await
        .expect("reverse impact")
    {
        ReverseImpact::Within(rows) => {
            let got: Vec<&str> = rows.iter().map(|r| r.gts_id.as_str()).collect();
            assert_eq!(
                got,
                vec![chain[0], chain[1]],
                "both dependents once each, gts_id-sorted, with the root excluded \
                 even though a row leads back to it, on {backend}"
            );
        }
        ReverseImpact::OverBound { at_least, bound } => {
            panic!("unexpected refusal on {backend}: {at_least} against a bound of {bound}")
        }
    }

    assert!(
        matches!(
            DependencyRepo::reverse_impact(&conn, &scope, &[ids[2]], 1)
                .await
                .expect("reverse impact"),
            ReverseImpact::OverBound { .. }
        ),
        "two dependents must exceed a bound of one on {backend}"
    );
}

/// A document too large for any `varchar`, so the column really is large text on
/// both backends. Built rather than written out: what matters is the size and that
/// it round-trips byte-identically.
fn oversized_schema(id: &str) -> String {
    let properties = (0..2000)
        .map(|i| format!("\"field_{i}\":{{\"type\":\"string\"}}"))
        .collect::<Vec<String>>()
        .join(",");
    format!(
        "{{\"$id\":\"gts://{id}\",\"$schema\":\"http://json-schema.org/draft-07/schema#\",\"type\":\"object\",\"properties\":{{{properties}}}}}"
    )
}

/// The read the transient store is built from (T5): each entity's **current**
/// revision, never its history, and the whole document.
///
/// Two things need a real backend. The pair disjunction binds two parameters per
/// entity rather than one, so a backend whose parameter limit or placeholder
/// syntax differed would fail here and nowhere else. And the authored document is
/// `text` / `LONGTEXT`: a document past any `varchar` bound proves the column
/// type, which a typeless `SQLite` cannot.
async fn current_documents_reads_the_current_revision_only(
    db: &Provider,
    family_id: i64,
    backend: &str,
) {
    let conn = db.conn().expect("conn");
    let scope = allow_all();

    let revised_id = gts_id!("acme.crm.revised.type.v1~");
    let single_id = gts_id!("acme.crm.single.type.v1~");
    let revised = EntityRepo::insert(&conn, &scope, new_entity(revised_id, family_id))
        .await
        .expect("insert revised entity")
        .expect("the identifier is free");
    let single = EntityRepo::insert(&conn, &scope, new_entity(single_id, family_id))
        .await
        .expect("insert single-revision entity")
        .expect("the identifier is free");

    let first = r#"{"$schema":"http://json-schema.org/draft-07/schema#","title":"first"}"#;
    let second = oversized_schema(revised_id);
    let only = r#"{"$schema":"http://json-schema.org/draft-07/schema#","title":"only"}"#;

    let item1 = seed_operation_item(&conn, revised_id, 1, NOW).await;
    seed_type_schema_revision(&conn, revised.id, 1, item1, first, NOW).await;
    seed_current_type_schema(&conn, revised.id, 1, first, NOW).await;
    let item2 = seed_operation_item(&conn, revised_id, 2, NOW).await;
    seed_type_schema_revision(&conn, revised.id, 2, item2, &second, NOW).await;
    type_schema::Entity::update_many()
        .secure()
        .col_expr(type_schema::Column::RevisionNo, Expr::value(2_i32))
        .filter(Condition::all().add(type_schema::Column::EntityId.eq(revised.id)))
        .scope_with(&scope)
        .exec(&conn)
        .await
        .expect("repoint the current revision");

    let item3 = seed_operation_item(&conn, single_id, 1, NOW).await;
    seed_type_schema_revision(&conn, single.id, 1, item3, only, NOW).await;
    seed_current_type_schema(&conn, single.id, 1, only, NOW).await;

    let docs = TypeSchemaRepo::current_documents(&conn, &scope, &[revised.id, single.id])
        .await
        .expect("current documents");

    assert_eq!(
        docs.len(),
        2,
        "one document per entity, not one per revision, on {backend}"
    );
    let revised_doc = docs
        .iter()
        .find(|d| d.entity_id == revised.id)
        .expect("the revised entity's current document");
    assert_eq!(
        revised_doc.revision_no, 2,
        "the current pointer selects the revision on {backend}"
    );
    assert_eq!(
        revised_doc.raw_schema, second,
        "a document past any varchar bound must round-trip byte-identically on {backend}"
    );
    assert!(revised_doc.raw_schema.len() > 60_000);
    let single_doc = docs
        .iter()
        .find(|d| d.entity_id == single.id)
        .expect("the single-revision entity's document");
    assert_eq!(single_doc.raw_schema, only);
}

async fn current_schemas_reads_every_named_entity_that_has_one(
    db: &Provider,
    family_id: i64,
    backend: &str,
) {
    let conn = db.conn().expect("conn");
    let scope = allow_all();

    let first_id = gts_id!("acme.crm.vector_a.type.v1~");
    let second_id = gts_id!("acme.crm.vector_b.type.v1~");
    let bare_id = gts_id!("acme.crm.vector_c.type.v1~");
    let mut inserted = Vec::new();
    for id in [first_id, second_id, bare_id] {
        inserted.push(
            EntityRepo::insert(&conn, &scope, new_entity(id, family_id))
                .await
                .expect("insert")
                .expect("the identifier is free"),
        );
    }

    let body = r#"{"$schema":"http://json-schema.org/draft-07/schema#","title":"vector"}"#;
    for (row, id) in inserted.iter().take(2).zip([first_id, second_id]) {
        let item = seed_operation_item(&conn, id, 1, NOW).await;
        seed_type_schema_revision(&conn, row.id, 1, item, body, NOW).await;
        seed_current_type_schema(&conn, row.id, 1, body, NOW).await;
    }

    let ids: Vec<i64> = inserted.iter().map(|row| row.id).collect();
    let states = TypeSchemaRepo::current_states(&conn, &scope, &ids)
        .await
        .expect("current states");

    assert_eq!(
        states.iter().map(|row| row.entity_id).collect::<Vec<_>>(),
        ids[..2].to_vec(),
        "one row per entity that has one, entity_id-sorted, and the third simply \
         absent on {backend}"
    );
    for row in &states {
        assert_eq!(
            row.resolution_fingerprint,
            vec![0x11],
            "the fingerprint must round-trip byte-identically on {backend}: the \
             guard compares it for equality and nothing else"
        );
    }

    assert!(
        TypeSchemaRepo::current_states(&conn, &scope, &[])
            .await
            .expect("empty read")
            .is_empty(),
        "an empty id set is an empty read, not a full scan, on {backend}"
    );
}

/// The two revision writes, on a real backend — neither demonstrable on `SQLite`.
///
/// `update_current` rebinds `resolution_fingerprint` in an `UPDATE`: a binary column
/// that is `BYTEA` / `BINARY(16)` here and typeless `BLOB` there, the exact class of
/// defect the uuid binding was. And `mark_item_unchanged` writes the one
/// `operation_item` state whose CHECK arm nothing else reaches, which `SQLite` has
/// never had a row to enforce against.
async fn a_revision_moves_the_pointer_and_can_report_unchanged(
    db: &Provider,
    family_id: i64,
    backend: &str,
) {
    let conn = db.conn().expect("conn");
    let scope = allow_all();

    let gts_id = gts_id!("acme.crm.moved.type.v1~");
    let entity = EntityRepo::insert(&conn, &scope, new_entity(gts_id, family_id))
        .await
        .expect("insert")
        .expect("the identifier is free");

    let first = r#"{"$schema":"http://json-schema.org/draft-07/schema#","title":"first"}"#;
    let second = r#"{"$schema":"http://json-schema.org/draft-07/schema#","title":"second"}"#;
    let item1 = seed_operation_item(&conn, gts_id, 1, NOW).await;
    seed_type_schema_revision(&conn, entity.id, 1, item1, first, NOW).await;
    seed_current_type_schema(&conn, entity.id, 1, first, NOW).await;
    let item2 = seed_operation_item(&conn, gts_id, 2, NOW).await;
    seed_type_schema_revision(&conn, entity.id, 2, item2, second, NOW).await;

    // A fingerprint that is not the seed's, so a pointer move that failed to
    // rebind the binary column would read back as the old value rather than as
    // an absent one.
    let fingerprint = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF];
    assert!(
        TypeSchemaRepo::update_current(
            &conn,
            &scope,
            NewCurrentTypeSchema {
                entity_id: entity.id,
                revision_no: 2,
                resolved_schema: second.to_owned(),
                effective_traits: r#"{"moved":true}"#.to_owned(),
                effective_traits_schema: "{}".to_owned(),
                resolution_fingerprint: fingerprint.clone(),
                now: NOW,
            },
            CurrentSchemaCas {
                revision_no: 1,
                resolution_fingerprint: vec![0x11],
            },
        )
        .await
        .expect("move the pointer"),
        "the update must match exactly one row on {backend}"
    );

    let current = TypeSchemaRepo::find_current(&conn, &scope, entity.id)
        .await
        .expect("read")
        .expect("a current row exists");
    assert_eq!(current.revision_no, 2, "the pointer moved on {backend}");
    assert_eq!(
        current.resolution_fingerprint, fingerprint,
        "the binary artifact must round-trip through an UPDATE on {backend}"
    );
    assert_eq!(current.resolved_schema, second);

    // An entity with no current row: the caller's existence recheck is missing,
    // and the repository says so rather than inventing a row.
    let orphan_id = gts_id!("acme.crm.orphan.type.v1~");
    let orphan = EntityRepo::insert(&conn, &scope, new_entity(orphan_id, family_id))
        .await
        .expect("insert")
        .expect("the identifier is free");
    assert!(
        !TypeSchemaRepo::update_current(
            &conn,
            &scope,
            NewCurrentTypeSchema {
                entity_id: orphan.id,
                revision_no: 1,
                resolved_schema: first.to_owned(),
                effective_traits: "{}".to_owned(),
                effective_traits_schema: "{}".to_owned(),
                resolution_fingerprint: vec![0x01],
                now: NOW,
            },
            CurrentSchemaCas {
                revision_no: 1,
                resolution_fingerprint: vec![0x11],
            },
        )
        .await
        .expect("the miss is reported, not raised"),
        "no current row means no rows affected on {backend}"
    );

    unchanged_is_written_once(db, gts_id, backend).await;
}

/// The `unchanged` terminal state, which is the one `ck_tr_operation_item_state`
/// arm nothing else in the suite reaches: `status = 4` with a null
/// `result_revision_no`, a non-null `result_resource_version` and
/// `expected_resource_version >= 1`.
async fn unchanged_is_written_once(db: &Provider, gts_id: &str, backend: &str) {
    let conn = db.conn().expect("conn");
    let scope = allow_all();
    let pending = seed_pending_revision_item(&conn, gts_id, 1, NOW).await;
    assert!(
        OperationRepo::mark_item_unchanged(&conn, &scope, pending, 1, NOW)
            .await
            .expect("the unchanged state must satisfy ck_tr_operation_item_state"),
        "the CAS must match the pending item on {backend}"
    );
    assert!(
        !OperationRepo::mark_item_unchanged(&conn, &scope, pending, 1, NOW)
            .await
            .expect("a second write reports failure rather than erroring"),
        "a terminal item may not be terminalized twice on {backend}"
    );
}

/// Everything the two backend tests assert, in one body so neither backend can
/// drift into covering less than the other.
/// A read under [`snapshot_read`] does not observe a commit that lands mid-read,
/// and the same pair of reads on a pooled connection does.
///
/// The property the service read paths depend on: `entity()` reads the entity row,
/// then its current-state artifacts, then its authored document, and a revision
/// committed between any two would compose a response out of two states. The control
/// half matters as much as the assertion — without it the test would pass against
/// the `READ COMMITTED` transaction `PostgreSQL` gives by default.
///
/// Mutation-checked: `TxConfig::default()` in place of `snapshot_read()` fails this
/// on `PostgreSQL` with `(1, 2)` and still passes on `MySQL`, whose InnoDB default is
/// already `REPEATABLE READ`.
async fn snapshot_read_does_not_see_a_mid_read_commit(
    db: &Provider,
    family_id: i64,
    backend: &str,
) {
    const ID: &str = gts_id!("acme.crm.snapshot.type.v1~");

    let entity = {
        let conn = db.conn().expect("conn");
        EntityRepo::insert(&conn, &allow_all(), new_entity(ID, family_id))
            .await
            .expect("seed the entity")
            .expect("the identifier is free")
    };

    // A second writer, on its own connection, bumps the row while the reader is
    // between its two reads.
    let writer = Arc::clone(db);
    // The closure is quantified over any transaction lifetime, so everything it
    // captures must be `'static` — hence the owned copy of `backend`.
    let named = backend.to_owned();
    let both = db
        .transaction_with_config(snapshot_read(&db.db()), move |tx| {
            Box::pin(async move {
                let first = EntityRepo::find_by_gts_id(tx, &allow_all(), ID)
                    .await?
                    .expect("first read");

                // The writer runs in its own task on purpose: `conn()` carries a
                // task-local guard that refuses a pooled connection while this
                // task holds a transaction (`DbError::ConnRequestedInsideTx`), so
                // a same-task write would not even reach the database. A separate
                // task is also the shape being modelled — another pod committing
                // while this read is in flight.
                let expected = first.resource_version;
                let moved = tokio::spawn(async move {
                    let conn = writer.conn()?;
                    EntityRepo::compare_and_swap_version(
                        &conn,
                        &allow_all(),
                        entity.id,
                        expected,
                        NOW,
                    )
                    .await
                    .map_err(DbError::from)
                })
                .await
                .expect("writer task")?;
                assert_eq!(
                    moved,
                    Some(expected + 1),
                    "the concurrent writer must win its CAS on {named}"
                );

                let second = EntityRepo::find_by_gts_id(tx, &allow_all(), ID)
                    .await?
                    .expect("second read");
                Ok((first.resource_version, second.resource_version))
            })
        })
        .await
        .expect("snapshot read transaction");

    assert_eq!(
        both.0, both.1,
        "both reads must come from one snapshot on {backend}, got {both:?}",
    );

    // The control: the same two reads without a transaction do observe the commit,
    // which is what makes the assertion above about isolation rather than about
    // nothing having changed.
    let conn = db.conn().expect("conn");
    let before = EntityRepo::find_by_gts_id(&conn, &allow_all(), ID)
        .await
        .expect("read")
        .expect("row");
    let moved = EntityRepo::compare_and_swap_version(
        &conn,
        &allow_all(),
        entity.id,
        before.resource_version,
        NOW,
    )
    .await
    .expect("cas");
    assert_eq!(
        moved,
        Some(before.resource_version + 1),
        "control CAS must succeed on {backend}"
    );
    let after = EntityRepo::find_by_gts_id(&conn, &allow_all(), ID)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        after.resource_version,
        before.resource_version + 1,
        "a pooled-connection read must see the new version on {backend}",
    );
}

/// The same race, but with every caller **inside a transaction** — which is the only
/// shape production ever uses: `create_or_get` is called from the admission commit
/// transaction, never from a pooled connection.
///
/// The case [`family_race_yields_one_row`] cannot cover: a *raised* unique violation
/// aborts the transaction on `PostgreSQL` — *"current transaction is aborted"* — so
/// the recovering re-read fails, while the pooled-connection version passes.
/// Absorbing the conflict (`ON CONFLICT DO NOTHING`) leaves the transaction usable,
/// and `commit_write` keeps `MySQL` from answering the re-read out of a pre-race
/// snapshot.
async fn family_race_inside_a_transaction_yields_one_row(db: &Provider, backend: &str) -> i64 {
    let mut handles = Vec::new();
    for _ in 0..8 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            let provider: DBProvider<DbError> = DBProvider::new(db.db());
            provider
                .transaction_with_config(commit_write(&db.db()), |tx| {
                    Box::pin(async move {
                        let (family, created) = VersionFamilyRepo::create_or_get(
                            tx,
                            &allow_all(),
                            TX_FAMILY,
                            OwnershipScope::Global,
                            None,
                            NOW,
                        )
                        .await?;
                        // A second statement in the same transaction, so an aborted
                        // one cannot pass by doing nothing after the conflict.
                        let read = VersionFamilyRepo::find_by_key(tx, &allow_all(), TX_FAMILY)
                            .await?
                            .expect("the family is readable in the same transaction");
                        assert_eq!(read.id, family.id);
                        Ok::<_, DbError>((family.id, created))
                    })
                })
                .await
                .expect("no caller may see an aborted transaction")
        }));
    }

    let mut ids = Vec::new();
    let mut creators = 0;
    for handle in handles {
        let (id, created) = handle.await.expect("task");
        ids.push(id);
        if created {
            creators += 1;
        }
    }
    assert_eq!(
        creators, 1,
        "exactly one caller may create the family inside a transaction on {backend}"
    );
    assert!(
        ids.windows(2).all(|w| w[0] == w[1]),
        "every caller must agree on the family row on {backend}: {ids:?}"
    );
    ids[0]
}

/// Concurrent creation of one entity, each inside its own transaction: one winner,
/// and the losers are told the identifier is taken rather than being handed a
/// database error.
///
/// `insert_entity` handled no conflict at all before, so a losing admission returned
/// a `WorkerError` — a `500` — where the item's outcome should have been
/// `already_exists`. `None` is that outcome, and the transaction it is reported in is
/// still usable, which is what lets the caller record it.
async fn entity_race_inside_a_transaction_has_one_winner(
    db: &Provider,
    family_id: i64,
    backend: &str,
) {
    let mut handles = Vec::new();
    for _ in 0..8 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            let provider: DBProvider<DbError> = DBProvider::new(db.db());
            provider
                .transaction_with_config(commit_write(&db.db()), move |tx| {
                    Box::pin(async move {
                        let inserted = EntityRepo::insert(
                            tx,
                            &allow_all(),
                            new_entity(TX_RACED_ID, family_id),
                        )
                        .await?;
                        // Again a statement after the conflict, in the same
                        // transaction: this is the assertion about abortion.
                        let read = EntityRepo::find_by_gts_id(tx, &allow_all(), TX_RACED_ID)
                            .await?
                            .expect("the winner's row is readable here");
                        Ok::<_, DbError>((inserted.is_some(), read.gts_id))
                    })
                })
                .await
                .expect("a lost race must not surface as a database error")
        }));
    }

    let mut winners = 0;
    for handle in handles {
        let (inserted, gts_id) = handle.await.expect("task");
        assert_eq!(gts_id, TX_RACED_ID);
        if inserted {
            winners += 1;
        }
    }
    assert_eq!(
        winners, 1,
        "exactly one caller may create the entity on {backend}"
    );
}

async fn assert_repo_primitives_behave(db: &Provider, backend: &str) {
    family_race_yields_one_row(db, backend).await;

    let conn = db.conn().expect("conn");
    let (family, created) = VersionFamilyRepo::create_or_get(
        &conn,
        &allow_all(),
        FAMILY_KEY,
        OwnershipScope::Global,
        None,
        NOW,
    )
    .await
    .expect("family");
    assert!(
        !created,
        "the race above already created the family on {backend}"
    );

    keyset_pages_in_byte_order(db, family.id, backend).await;
    pattern_list_agrees_with_gts(db, backend).await;
    cas_reports_by_affected_rows(db, family.id, backend).await;
    closure_walks_a_chain(db, family.id, backend).await;
    reverse_impact_walks_back_up_a_chain(db, family.id, backend).await;
    current_documents_reads_the_current_revision_only(db, family.id, backend).await;
    current_schemas_reads_every_named_entity_that_has_one(db, family.id, backend).await;
    a_revision_moves_the_pointer_and_can_report_unchanged(db, family.id, backend).await;
    snapshot_read_does_not_see_a_mid_read_commit(db, family.id, backend).await;

    // Last, because both write rows the enumerating tests above would have to
    // account for.
    let tx_family = family_race_inside_a_transaction_yields_one_row(db, backend).await;
    entity_race_inside_a_transaction_has_one_winner(db, tx_family, backend).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repository_primitives_behave_on_postgres() {
    use testcontainers::ImageExt;
    use testcontainers::runners::AsyncRunner;

    let request = test_containers::postgres()
        .with_env_var("POSTGRES_PASSWORD", "pass")
        .with_env_var("POSTGRES_USER", "user")
        .with_env_var("POSTGRES_DB", "app");
    let container = request.start().await.expect("start postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port");
    let host = container
        .get_host()
        .await
        .expect("postgres container host")
        .to_string();
    wait_for_tcp(host.trim_matches(['[', ']']), port, Duration::from_mins(1)).await;

    let db = provider_for(&format!("postgres://user:pass@{host}:{port}/app"), 8).await;
    assert_repo_primitives_behave(&db, "postgres").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repository_primitives_behave_on_mysql() {
    use testcontainers::runners::AsyncRunner;

    let container = test_containers::mysql()
        .start()
        .await
        .expect("start mysql container");
    let port = container
        .get_host_port_ipv4(3306)
        .await
        .expect("mysql port");
    let host = container
        .get_host()
        .await
        .expect("mysql container host")
        .to_string();
    wait_for_tcp(host.trim_matches(['[', ']']), port, Duration::from_mins(2)).await;

    let db = provider_for(&format!("mysql://root@{host}:{port}/test"), 8).await;
    assert_repo_primitives_behave(&db, "mysql").await;
}
