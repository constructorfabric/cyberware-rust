//! The transient `gts-rust` store built from real rows (SPEC D2, §8.2, T5).
//!
//! What only a database can show is here: that the store is built from the
//! `dependency` closure and *nothing outside it*, that the candidate overlay wins
//! over a committed document, and that two sequential builds each see the current
//! revision with no invalidation step between them. The order and refusal rules
//! are pinned by the in-source tests beside `domain/gts_store.rs`.
//!
//! Fixtures write `type_schema` and `type_schema_revision` through their
//! `ActiveModel`s rather than through a repository, because no repository writes
//! them yet: those writes belong to the admission worker (T8), which is their
//! first caller. `entity_test.rs` seeds the same rows the same way.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::secure::SecureUpdateExt;
use toolkit_db::{DBProvider, DbError};
use toolkit_gts::gts_id;
use uuid::Uuid;

use types_registry::domain::enums::{DependencyKind, EntityKind, OwnershipScope};
use types_registry::domain::gts_store::{
    StoreBuildError, UnitDocument, UnitStore, load_unit_store,
};
use types_registry::domain::ports::{NewEntity, snapshot_read};
use types_registry::infra::storage::entity::{type_schema, type_schema_revision};
use types_registry::infra::storage::repo::{DependencyRepo, EntityRepo, VersionFamilyRepo};

mod common;
use common::{
    allow_all, seed_current_type_schema, seed_operation_item, seed_type_schema_revision, stores,
    test_db,
};

const NOW: OffsetDateTime = datetime!(2026-08-18 09:15:30 UTC);
const FAMILY_KEY: &str = "gts.acme.crm.customer.type";
const BASE: &str = gts_id!("acme.crm.customer.type.v1~");
const DERIVED: &str = gts_id!("acme.crm.customer.type.v1~acme.crm.premium.type.v1~");
const STRANGER: &str = gts_id!("acme.crm.order.type.v1~");
const INVOICE: &str = gts_id!("acme.crm.invoice.type.v1~");
const CONSUMER: &str = gts_id!("acme.crm.report.type.v1~");
const ABSENT: &str = gts_id!("acme.crm.ghost.type.v1~");

type Provider = Arc<DBProvider<DbError>>;

/// A base document: one property, its own dialect and `$id`.
fn base_schema(id: &str, property: &str) -> Value {
    json!({
        "$id": format!("gts://{id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { property: { "type": "string" } },
    })
}

/// A document that consumes `base` through a `gts://` `$ref` — the shape the
/// dependency closure exists for.
fn derived_schema(id: &str, base: &str) -> Value {
    json!({
        "$id": format!("gts://{id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "allOf": [
            { "$ref": format!("gts://{base}") },
            { "type": "object", "properties": { "tier": { "type": "string" } } },
        ],
    })
}

fn referencing_schema(id: &str, target: &str) -> Value {
    json!({
        "$id": format!("gts://{id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { "subject": { "$ref": format!("gts://{target}") } },
    })
}

fn doc(gts_id: &str, content: Value) -> UnitDocument {
    UnitDocument {
        gts_id: gts_id.to_owned(),
        content,
    }
}

/// `load_unit_store` runs in the caller's transaction, so every test that is not
/// specifically about transaction shape goes through one snapshot read — the same
/// `ports::snapshot_read()` the worker uses, so the tests exercise the real
/// isolation rather than a weaker one.
///
/// The build's own error is carried out as the transaction's *value*: a
/// `StoreBuildError` is an outcome under test, not a reason to roll back.
async fn load_in_snapshot(
    db: &Provider,
    candidates: Vec<UnitDocument>,
) -> Result<UnitStore, StoreBuildError> {
    db.transaction_with_config(snapshot_read(&db.db()), move |tx| {
        Box::pin(async move {
            Ok(load_unit_store(stores().as_ref(), tx, &allow_all(), candidates).await)
        })
    })
    .await
    .expect("snapshot read transaction")
}

/// One family, shared by every fixture.
async fn seed_family(db: &Provider) -> i64 {
    let conn = db.conn().expect("conn");
    let (family, _) = VersionFamilyRepo::create_or_get(
        &conn,
        &allow_all(),
        FAMILY_KEY,
        OwnershipScope::Global,
        None,
        NOW,
    )
    .await
    .expect("family");
    family.id
}

/// An admitted Type Schema: entity row, immutable revision 1, current pointer.
async fn seed_schema(db: &Provider, family_id: i64, gts_id: &str, content: &Value) -> i64 {
    let conn = db.conn().expect("conn");
    let scope = allow_all();

    let ent = EntityRepo::insert(
        &conn,
        &scope,
        NewEntity {
            gts_uuid: Uuid::new_v5(&Uuid::NAMESPACE_URL, gts_id.as_bytes()),
            gts_id: gts_id.to_owned(),
            entity_kind: EntityKind::TypeSchema,
            family_id,
            ownership_scope: OwnershipScope::Global,
            owner_tenant_id: None,
            owning_gear: Some("types-registry".to_owned()),
            now: NOW,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("insert entity {gts_id}: {e}"))
    .expect("the identifier is free");

    let item_id = seed_operation_item(&conn, gts_id, 1, NOW).await;
    seed_type_schema_revision(&conn, ent.id, 1, item_id, &content.to_string(), NOW).await;
    seed_current_type_schema(&conn, ent.id, 1, &content.to_string(), NOW).await;

    ent.id
}

/// Admit a second revision and move the current pointer to it — what an ordinary
/// content revision does, done directly so the test observes a *committed* change
/// the builder never learned about through a callback.
async fn commit_new_revision(db: &Provider, entity_id: i64, gts_id: &str, content: &Value) {
    let conn = db.conn().expect("conn");
    let item_id = seed_operation_item(&conn, gts_id, 2, NOW).await;
    seed_type_schema_revision(&conn, entity_id, 2, item_id, &content.to_string(), NOW).await;

    type_schema::Entity::update_many()
        .secure()
        .col_expr(type_schema::Column::RevisionNo, Expr::value(2_i32))
        .col_expr(
            type_schema::Column::ResolvedSchema,
            Expr::value(content.to_string()),
        )
        .filter(Condition::all().add(type_schema::Column::EntityId.eq(entity_id)))
        .scope_with(&allow_all())
        .exec(&conn)
        .await
        .expect("repoint the current revision");
}

async fn add_edge(db: &Provider, from: i64, to: i64) {
    let conn = db.conn().expect("conn");
    DependencyRepo::replace_outgoing(
        &conn,
        &allow_all(),
        from,
        &[(DependencyKind::SchemaRef, to)],
    )
    .await
    .expect("write the edge");
}

// ---------------------------------------------------------------------------
// The store resolves what the closure supplied
// ---------------------------------------------------------------------------

/// A candidate that consumes a committed base: the closure supplies the base, and
/// the derived document's `gts://` reference resolves against it. Without the
/// closure read this is `UnresolvedRefs`.
#[tokio::test]
async fn a_chained_fixture_resolves_the_derived_schemas_base() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    let base_id = seed_schema(&db, family, BASE, &base_schema(BASE, "name")).await;
    let derived_content = derived_schema(DERIVED, BASE);
    let derived_id = seed_schema(&db, family, DERIVED, &derived_content).await;
    add_edge(&db, derived_id, base_id).await;

    let mut unit = load_in_snapshot(&db, vec![doc(DERIVED, derived_content.clone())])
        .await
        .expect("build the unit store");

    // The base loads before the derived schema that consumes it.
    assert_eq!(unit.load_order(), [BASE, DERIVED]);

    let resolved = unit
        .store_mut()
        .resolve_schema_refs(&derived_content)
        .expect("the base must be resolvable from the store");
    let text = resolved.to_string();
    assert!(
        !text.contains("$ref"),
        "every gts:// reference must be inlined, got {text}",
    );
    assert!(
        text.contains("\"name\""),
        "the base's own property must appear in the resolved document, got {text}",
    );
    // The document's own `$id` is a `gts://` URI and stays: `resolve_schema_refs`
    // inlines references, it does not rewrite identity.
    assert!(text.contains(&format!("gts://{DERIVED}")), "got {text}");
}

/// Closure containment. A document that the candidate does not reach through
/// `dependency` is **absent** — which is what an accidental whole-table load
/// would break.
#[tokio::test]
async fn a_document_outside_the_closure_is_absent_from_the_store() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    let base_id = seed_schema(&db, family, BASE, &base_schema(BASE, "name")).await;
    let derived_content = derived_schema(DERIVED, BASE);
    let derived_id = seed_schema(&db, family, DERIVED, &derived_content).await;
    add_edge(&db, derived_id, base_id).await;
    // Committed, active, in the same family — and reachable from nothing.
    seed_schema(&db, family, STRANGER, &base_schema(STRANGER, "sku")).await;

    let mut unit = load_in_snapshot(&db, vec![doc(DERIVED, derived_content)])
        .await
        .expect("build the unit store");

    let store = unit.store_mut();
    assert!(store.get(BASE).is_some(), "the closure supplies the base");
    assert!(store.get(DERIVED).is_some());
    assert!(
        store.get(STRANGER).is_none(),
        "a document outside the closure must not be in the store",
    );
    assert_eq!(store.items().count(), 2);
}

/// A first admission: the candidate has no entity row at all, so the closure
/// walks nothing. It is reported as a missing candidate rather than failing the
/// read, and the store holds only the candidate.
#[tokio::test]
async fn a_candidate_with_no_dependencies_yields_a_store_holding_only_it() {
    let db = test_db().await;
    seed_family(&db).await;

    let mut unit = load_in_snapshot(&db, vec![doc(BASE, base_schema(BASE, "name"))])
        .await
        .expect("build the unit store");

    assert_eq!(unit.missing_candidates(), [BASE]);
    assert_eq!(unit.load_order(), [BASE]);
    assert_eq!(unit.store_mut().items().count(), 1);
}

// ---------------------------------------------------------------------------
// Nothing is retained between builds
// ---------------------------------------------------------------------------

/// The store holds no state between units. Two sequential builds across a
/// committed revision each observe the current one, with no invalidation step,
/// no rebuild hook and nothing to notify — because each build re-reads.
#[tokio::test]
async fn two_sequential_builds_each_observe_the_committed_revision() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    let base_id = seed_schema(&db, family, BASE, &base_schema(BASE, "name")).await;
    let derived_content = derived_schema(DERIVED, BASE);
    let derived_id = seed_schema(&db, family, DERIVED, &derived_content).await;
    add_edge(&db, derived_id, base_id).await;

    let mut first = load_in_snapshot(&db, vec![doc(DERIVED, derived_content.clone())])
        .await
        .expect("first build");
    let before = first
        .store_mut()
        .get_schema_content(BASE)
        .expect("the base is in the first store");
    assert!(before.to_string().contains("\"name\""));

    commit_new_revision(&db, base_id, BASE, &base_schema(BASE, "renamed")).await;

    let mut second = load_in_snapshot(&db, vec![doc(DERIVED, derived_content)])
        .await
        .expect("second build");
    let after = second
        .store_mut()
        .get_schema_content(BASE)
        .expect("the base is in the second store");
    assert!(
        after.to_string().contains("\"renamed\""),
        "the second build must read the new revision, got {after}",
    );
    assert!(
        !after.to_string().contains("\"name\""),
        "the second build must not carry the old revision over, got {after}",
    );
}

/// The candidate overlay wins over the committed document under the same
/// identifier: an in-batch reference must resolve against what is being admitted,
/// never against a previously committed revision.
#[tokio::test]
async fn the_candidate_overlay_beats_the_committed_document() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    seed_schema(&db, family, BASE, &base_schema(BASE, "committed")).await;

    let mut unit = load_in_snapshot(&db, vec![doc(BASE, base_schema(BASE, "candidate"))])
        .await
        .expect("build the unit store");

    assert!(
        unit.missing_candidates().is_empty(),
        "the candidate has an entity row",
    );
    let content = unit
        .store_mut()
        .get_schema_content(BASE)
        .expect("the candidate is in the store");
    let text = content.to_string();
    assert!(text.contains("\"candidate\""), "got {text}");
    assert!(!text.contains("\"committed\""), "got {text}");
    assert_eq!(unit.store_mut().items().count(), 1);
}

// ---------------------------------------------------------------------------
// Boundaries
// ---------------------------------------------------------------------------

/// A tombstoned base still loads. It remains the compatibility baseline until
/// purge (T4's closure keeps it deliberately), so a store that dropped it would
/// let an ordinary deletion move the baseline.
#[tokio::test]
async fn a_tombstoned_base_still_loads() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    let base_id = seed_schema(&db, family, BASE, &base_schema(BASE, "name")).await;
    let derived_content = derived_schema(DERIVED, BASE);
    let derived_id = seed_schema(&db, family, DERIVED, &derived_content).await;
    add_edge(&db, derived_id, base_id).await;

    let conn = db.conn().expect("conn");
    assert_eq!(
        EntityRepo::mark_deleted(&conn, &allow_all(), base_id, 1, NOW)
            .await
            .expect("tombstone the base"),
        Some(2)
    );

    let mut unit = load_in_snapshot(&db, vec![doc(DERIVED, derived_content)])
        .await
        .expect("build the unit store");
    assert!(unit.store_mut().get(BASE).is_some());
}

/// A Type Schema entity with no current document is a corrupt row, not a race —
/// `type_schema` is written in the same transaction as the entity (D3). Named
/// rather than silently skipped, because a silent skip would surface later as
/// `UnresolvedRefs` against a document nobody can point at.
#[tokio::test]
async fn an_entity_with_no_current_document_is_named() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    let derived_content = derived_schema(DERIVED, BASE);
    let derived_id = seed_schema(&db, family, DERIVED, &derived_content).await;

    // A base entity with no `type_schema` / `type_schema_revision` rows at all.
    let conn = db.conn().expect("conn");
    let orphan = EntityRepo::insert(
        &conn,
        &allow_all(),
        NewEntity {
            gts_uuid: Uuid::new_v5(&Uuid::NAMESPACE_URL, BASE.as_bytes()),
            gts_id: BASE.to_owned(),
            entity_kind: EntityKind::TypeSchema,
            family_id: family,
            ownership_scope: OwnershipScope::Global,
            owner_tenant_id: None,
            // `ck_tr_entity_owner` requires attribution on a global entity.
            owning_gear: Some("types-registry".to_owned()),
            now: NOW,
        },
    )
    .await
    .expect("insert the orphan entity")
    .expect("the identifier is free");
    add_edge(&db, derived_id, orphan.id).await;

    let err = load_in_snapshot(&db, vec![doc(DERIVED, derived_content)])
        .await
        .expect_err("a document-less entity must be reported");
    match err {
        StoreBuildError::MissingDocument { gts_id } => assert_eq!(gts_id, BASE),
        other => panic!("expected MissingDocument, got {other}"),
    }
}

/// A stored document that is not JSON names its entity. It cannot happen through
/// admission — the acceptance path parses before it stores — so the value of the
/// test is that the failure is attributable rather than a bare parse error.
#[tokio::test]
async fn a_stored_document_that_is_not_json_names_its_entity() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    let base_id = seed_schema(&db, family, BASE, &base_schema(BASE, "name")).await;
    let derived_content = derived_schema(DERIVED, BASE);
    let derived_id = seed_schema(&db, family, DERIVED, &derived_content).await;
    add_edge(&db, derived_id, base_id).await;

    let conn = db.conn().expect("conn");
    type_schema_revision::Entity::update_many()
        .secure()
        .col_expr(
            type_schema_revision::Column::RawSchema,
            Expr::value("{ not json".to_owned()),
        )
        .filter(Condition::all().add(type_schema_revision::Column::EntityId.eq(base_id)))
        .scope_with(&allow_all())
        .exec(&conn)
        .await
        .expect("corrupt the stored document");

    let err = load_in_snapshot(&db, vec![doc(DERIVED, derived_content)])
        .await
        .expect_err("a malformed stored document must be reported");
    match err {
        StoreBuildError::Content { gts_id, .. } => assert_eq!(gts_id, BASE),
        other => panic!("expected Content, got {other}"),
    }
}

/// The builder composes inside a **write** transaction the caller opened, which is
/// what T13's edge writes and T19's batching will need: the store is built and the
/// rows are written under one transaction rather than two. Every other test here
/// uses the read-only snapshot instead, so this one exists to pin the write case.
#[tokio::test]
async fn the_builder_runs_inside_a_transaction() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    let base_id = seed_schema(&db, family, BASE, &base_schema(BASE, "name")).await;
    let derived_content = derived_schema(DERIVED, BASE);
    let derived_id = seed_schema(&db, family, DERIVED, &derived_content).await;
    add_edge(&db, derived_id, base_id).await;

    let loaded: Vec<String> = db
        .transaction(|tx| {
            let content = derived_content.clone();
            Box::pin(async move {
                let unit = load_unit_store(
                    stores().as_ref(),
                    tx,
                    &allow_all(),
                    vec![doc(DERIVED, content)],
                )
                .await
                .expect("build inside a transaction");
                Ok(unit.load_order().to_vec())
            })
        })
        .await
        .expect("transaction");

    assert_eq!(loaded, [BASE, DERIVED]);
}

#[tokio::test]
async fn a_reference_outside_the_identifier_chain_is_seeded_from_the_document() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    seed_schema(&db, family, STRANGER, &base_schema(STRANGER, "sku")).await;
    let candidate = referencing_schema(CONSUMER, STRANGER);

    let mut unit = load_in_snapshot(&db, vec![doc(CONSUMER, candidate.clone())])
        .await
        .expect("build the unit store");

    assert!(
        unit.store_mut().get(STRANGER).is_some(),
        "the target the document names must be in the store, load order {:?}",
        unit.load_order(),
    );
    let resolved = unit
        .store_mut()
        .resolve_schema_refs(&candidate)
        .expect("a reference the registry holds must resolve");
    assert!(
        resolved.to_string().contains("\"sku\""),
        "the target's own property must be inlined, got {resolved}",
    );
}

#[tokio::test]
async fn a_reference_no_entity_carries_is_a_missing_reference_not_a_missing_candidate() {
    let db = test_db().await;
    seed_family(&db).await;

    let unit = load_in_snapshot(
        &db,
        vec![doc(CONSUMER, referencing_schema(CONSUMER, ABSENT))],
    )
    .await
    .expect("an absent target does not fail the read");

    assert_eq!(
        unit.missing_candidates(),
        [CONSUMER],
        "the candidate's own absence is the ordinary first-admission state",
    );
    assert_eq!(unit.missing_references(), [ABSENT]);
}

#[tokio::test]
async fn a_revision_resolves_against_the_reference_it_now_carries() {
    let db = test_db().await;
    let family = seed_family(&db).await;
    let stranger_id = seed_schema(&db, family, STRANGER, &base_schema(STRANGER, "sku")).await;
    seed_schema(&db, family, INVOICE, &base_schema(INVOICE, "total")).await;
    let consumer_id = seed_schema(
        &db,
        family,
        CONSUMER,
        &referencing_schema(CONSUMER, STRANGER),
    )
    .await;
    add_edge(&db, consumer_id, stranger_id).await;

    let candidate = referencing_schema(CONSUMER, INVOICE);
    let mut unit = load_in_snapshot(&db, vec![doc(CONSUMER, candidate.clone())])
        .await
        .expect("build the unit store");

    assert!(
        unit.store_mut().get(INVOICE).is_some(),
        "the reference this revision adds must resolve, load order {:?}",
        unit.load_order(),
    );
    assert!(
        unit.missing_references().is_empty(),
        "both targets exist, so nothing is missing: {:?}",
        unit.missing_references(),
    );
    let resolved = unit
        .store_mut()
        .resolve_schema_refs(&candidate)
        .expect("the new reference must resolve");
    assert!(resolved.to_string().contains("\"total\""), "got {resolved}");
}
