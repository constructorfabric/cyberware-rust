//! The three non-stored family rules, end to end through the admission worker.
//!
//! Every test calls the worker directly: no `sleep`, no timer, no polling
//! (SPEC §13).
//!
//! # Concurrency: the lock is here, the unique-key race is not
//!
//! Covered here: a creation *holds* the family's advisory lock across its commit
//! transaction — probed rather than raced, so it needs no second writer.
//!
//! Not covered here: two simultaneous first registrations, decided by
//! `uq_tr_version_family_key`. `SQLite` cannot demonstrate that — a second
//! concurrent writer fails the whole transaction with `database is locked` rather
//! than losing a unique-key race — so `repo_backends_test.rs::family_race_*` covers
//! it on the real backends.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use sea_orm::EntityTrait;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::secure::SecureEntityExt;
use toolkit_db::{DBProvider, DbError, DbTx};
use toolkit_gts::gts_id;
use uuid::Uuid;

use types_registry::config::TypesRegistryConfig;
use types_registry::domain::admission::acceptance::{AcceptanceContext, AcceptanceError, accept};
use types_registry::domain::admission::worker::{
    OperationOutcome, Tuning, WorkerError, run_operation,
};
use types_registry::domain::admission::{Candidate, OperationDispatch, SubmitRequest};
use types_registry::domain::enums as domain_enums;
use types_registry::domain::policy::RegistrationPolicy;
use types_registry::infra::storage::entity::{dependency, entity, version_family};
use types_registry::infra::storage::repo::EntityRepo;

mod common;
use common::{allow_all, stores, test_db};

const NOW: OffsetDateTime = datetime!(2026-08-18 09:15:30 UTC);
const LATER: OffsetDateTime = datetime!(2026-08-18 10:20:40 UTC);

/// One family, spelled at every version the table below needs. `family_tests.rs`
/// proves purely that all of these derive `gts.cf.core.example.thing`.
const V1: &str = gts_id!("cf.core.example.thing.v1~");
const V1_0: &str = gts_id!("cf.core.example.thing.v1.0~");
const V1_1: &str = gts_id!("cf.core.example.thing.v1.1~");
const V1_2: &str = gts_id!("cf.core.example.thing.v1.2~");
const V2: &str = gts_id!("cf.core.example.thing.v2~");
const V2_0: &str = gts_id!("cf.core.example.thing.v2.0~");

struct NoDispatch;

#[async_trait::async_trait]
impl OperationDispatch for NoDispatch {
    async fn enqueue(&self, _tx: &DbTx<'_>, _operation_id: Uuid) -> anyhow::Result<()> {
        Ok(())
    }
}

fn schema(gts_id: &str) -> Value {
    json!({
        "$id": format!("gts://{gts_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { "name": { "type": "string" } },
    })
}

async fn submit(db: &Arc<DBProvider<DbError>>, key: &str, gts_id: &str) -> Uuid {
    let provider: DBProvider<AcceptanceError> = DBProvider::new(db.db());
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(NoDispatch);
    accept(
        &stores(),
        &provider,
        &allow_all(),
        &AcceptanceContext {
            policy: &policy,
            config: &config,
            metrics: &common::metrics(),
        },
        &dispatch,
        &SubmitRequest {
            idempotency_key: key.to_owned(),
            kind: domain_enums::OperationKind::Registration,
            dry_run: false,
            candidates: vec![Candidate {
                gts_id: gts_id.to_owned(),
                content: Some(schema(gts_id)),
                expected_resource_version: None,
                force: false,
            }],
        },
        NOW,
    )
    .await
    .expect("accepted")
    .operation_id
}

fn worker(db: &Arc<DBProvider<DbError>>) -> DBProvider<WorkerError> {
    DBProvider::new(db.db())
}

async fn admit(db: &Arc<DBProvider<DbError>>, key: &str, gts_id: &str) -> OperationOutcome {
    let op = submit(db, key, gts_id).await;
    run_operation(
        &stores(),
        &worker(db),
        &allow_all(),
        Tuning {
            limits: &common::limits(),
            worker: &common::worker_settings(),
            metrics: &common::metrics(),
        },
        op,
        LATER,
    )
    .await
    .expect("the worker itself must not fail")
}

/// The item's status and, when it was refused, the machine reason.
fn verdict(outcome: &OperationOutcome) -> (domain_enums::OperationItemStatus, Option<String>) {
    let item = &outcome.items[0];
    (
        item.status,
        item.failure.as_ref().map(|f| f.reason.to_string()),
    )
}

// ---------------------------------------------------------------------------
// Shape and contiguity, table-driven
// ---------------------------------------------------------------------------

/// `database.sql`'s three rules, one row each, plus the cases that pin their
/// **major** scope:
///
/// ```text
/// shape, minor-bearing candidate vM.n~   -> refuse while vM~ exists
/// shape, major-only candidate vM~        -> refuse while vM.0~ exists
/// contiguity, candidate vM.n~ with n > 0 -> refuse unless vM.(n-1)~ exists
/// ```
#[tokio::test]
async fn shape_and_contiguity_over_the_combinations() {
    /// One row of the table: the members already in the family, in admission
    /// order; the candidate that follows them; and the reason it is refused, or
    /// `None` when it must be admitted.
    struct Case {
        existing: &'static [&'static str],
        candidate: &'static str,
        refused_with: Option<&'static str>,
    }
    const fn case(
        existing: &'static [&'static str],
        candidate: &'static str,
        refused_with: Option<&'static str>,
    ) -> Case {
        Case {
            existing,
            candidate,
            refused_with,
        }
    }

    let cases = [
        // --- shape: a minor-bearing candidate under a major-only member ------
        case(&[V1], V1_0, Some("family_shape_conflict")),
        case(&[V1], V1_1, Some("family_shape_conflict")),
        // --- shape: a major-only candidate under a minor-bearing member ------
        case(&[V1_0], V1, Some("family_shape_conflict")),
        // --- contiguity ------------------------------------------------------
        case(&[V1_0], V1_1, None),
        case(&[V1_0], V1_2, Some("missing_predecessor")),
        case(&[V1_0, V1_1], V1_2, None),
        case(&[], V1_1, Some("missing_predecessor")),
        // --- the openings: M.0 and a bare major both found a major -----------
        case(&[], V1_0, None),
        case(&[], V1, None),
        // --- both rules are scoped to ONE major ------------------------------
        // A family may hold a major-only v1~ beside a minor-bearing v2.0~.
        case(&[V1], V2, None),
        case(&[V1], V2_0, None),
        case(&[V1_0], V2, None),
    ];

    for Case {
        existing,
        candidate,
        refused_with,
    } in cases
    {
        let db = test_db().await;
        for (i, member) in existing.iter().enumerate() {
            let outcome = admit(&db, &format!("seed-{i}"), member).await;
            assert_eq!(
                verdict(&outcome).0,
                domain_enums::OperationItemStatus::Succeeded,
                "the fixture member {member} must admit before the case runs",
            );
        }

        let (status, reason) = verdict(&admit(&db, "candidate", candidate).await);
        match refused_with {
            None => assert_eq!(
                status,
                domain_enums::OperationItemStatus::Succeeded,
                "{candidate} must admit into a family holding {existing:?}",
            ),
            Some(expected) => {
                assert_eq!(
                    status,
                    domain_enums::OperationItemStatus::Failed,
                    "{candidate} must be refused in a family holding {existing:?}",
                );
                assert_eq!(
                    reason.as_deref(),
                    Some(expected),
                    "{candidate} in a family holding {existing:?}",
                );
            }
        }
    }
}

/// A refused candidate writes nothing — not the entity, and not a revision.
#[tokio::test]
async fn a_family_refusal_leaves_no_entity_behind() {
    let db = test_db().await;
    admit(&db, "k1", V1).await;
    admit(&db, "k2", V1_0).await;

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    assert!(
        EntityRepo::find_by_gts_id(&conn, &allow_all(), V1_0)
            .await
            .expect("read")
            .is_none(),
        "a shape refusal must not leave the member it refused",
    );
}

// ---------------------------------------------------------------------------
// Tombstones
// ---------------------------------------------------------------------------

/// A `DELETED` predecessor still counts: its definition stays the compatibility
/// baseline until purge, so skipping it would let an ordinary deletion move the
/// baseline (`database.sql`, ADR-0013).
#[tokio::test]
async fn a_deleted_predecessor_still_satisfies_contiguity() {
    let db = test_db().await;
    admit(&db, "k1", V1_0).await;

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let scope = allow_all();
    let predecessor = EntityRepo::find_by_gts_id(&conn, &scope, V1_0)
        .await
        .expect("read")
        .expect("the predecessor was admitted");
    assert_eq!(
        EntityRepo::mark_deleted(
            &conn,
            &scope,
            predecessor.id,
            predecessor.resource_version,
            NOW
        )
        .await
        .expect("tombstone the predecessor"),
        Some(predecessor.resource_version + 1),
        "the fixture must really produce a tombstone"
    );

    let (status, reason) = verdict(&admit(&db, "k2", V1_1).await);
    assert_eq!(
        status,
        domain_enums::OperationItemStatus::Succeeded,
        "a tombstoned predecessor still counts, got {reason:?}",
    );
}

/// The mirror: a `DELETED` major-only member still blocks a minor-bearing
/// candidate.
#[tokio::test]
async fn a_deleted_member_still_decides_minor_shape() {
    let db = test_db().await;
    admit(&db, "k1", V1).await;

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let scope = allow_all();
    let existing = EntityRepo::find_by_gts_id(&conn, &scope, V1)
        .await
        .expect("read")
        .expect("admitted");
    assert_eq!(
        EntityRepo::mark_deleted(&conn, &scope, existing.id, existing.resource_version, NOW)
            .await
            .expect("tombstone the blocker"),
        Some(existing.resource_version + 1),
        "the fixture must really produce a tombstone"
    );
    // Re-read rather than trust the `bool`: without this the shape refusal below
    // would fire whether or not the tombstone was written, and the test's claim
    // would never be exercised.
    let tombstoned = EntityRepo::find_by_gts_id(&conn, &scope, V1)
        .await
        .expect("read")
        .expect("the tombstone stays reverse-resolvable");
    assert_eq!(
        tombstoned.lifecycle_status,
        domain_enums::LifecycleStatus::Deleted,
    );

    let (status, reason) = verdict(&admit(&db, "k2", V1_1).await);
    assert_eq!(status, domain_enums::OperationItemStatus::Failed);
    assert_eq!(reason.as_deref(), Some("family_shape_conflict"));
}

// ---------------------------------------------------------------------------
// The family row
// ---------------------------------------------------------------------------

/// Derivation maps every version onto **one row**, and the entity's owner columns
/// are a projection of that row rather than a second copy of the request.
#[tokio::test]
async fn one_family_row_holds_every_version_and_owns_its_members() {
    let db = test_db().await;
    admit(&db, "k1", V1).await;
    admit(&db, "k2", V2).await;

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let families = version_family::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("families");
    assert_eq!(
        families.len(),
        1,
        "v1~ and v2~ are two versions of one logical entity",
    );
    let family = &families[0];
    assert_eq!(family.family_key, "gts.cf.core.example.thing");

    let entities = entity::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("entities");
    assert_eq!(entities.len(), 2);
    for row in &entities {
        assert_eq!(row.family_id, family.id);
        assert_eq!(
            domain_enums::OwnershipScope::from(row.ownership_scope),
            family.ownership_scope.into(),
            "the entity's owner columns are a projection of the family row",
        );
        assert_eq!(row.owner_tenant_id, family.owner_tenant_id);
    }
}

/// A predecessor is **not** a dependency edge: such an edge would forbid deleting
/// `v1.0~` while `v1.1~` exists, which ADR-0008 permits and ADR-0004 relies on
/// (`database.sql`). This pins that the dependency table must not grow one.
#[tokio::test]
async fn a_predecessor_is_not_a_dependency_edge() {
    let db = test_db().await;
    admit(&db, "k1", V1_0).await;
    admit(&db, "k2", V1_1).await;

    let provider = worker(&db);
    let conn = provider.conn().expect("conn");
    let edges = dependency::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("edges");
    assert!(
        edges.is_empty(),
        "the implicit v1.0~ -> v1.1~ edge is never stored: {edges:?}",
    );
}

// ---------------------------------------------------------------------------
// The family lock
// ---------------------------------------------------------------------------
