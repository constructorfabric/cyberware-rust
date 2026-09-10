//! Schema-level tests for the P0 migrations (T2), run against a real
//! in-memory `SQLite` database (~1ms per DB). They verify the SQL itself —
//! every `CHECK` constraint, every `UNIQUE`, the composite foreign key that
//! ties an `operation_item` to its parent's `kind` / `dry_run`, and the
//! up/down/up roundtrip — without needing a running server.
//!
//! `docs/database.sql` is normative. P0 adds nine initial tables and then
//! `coordination_state`; federation's `source_claim` remains absent.
//!
//! Postgres- and `MySQL`-dialect *behaviour* (identity columns, `bytea`,
//! `ascii_bin` collation, FK `RESTRICT`) is not reachable from `SQLite`. The
//! per-backend statement lists are pinned by the in-source tests beside the
//! migration; real up/down on the other two backends is covered by
//! `make test-pg` / `make test-mysql` once Docker is available.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use toolkit_gts::gts_id;

use types_registry::infra::storage::Migrator;

/// Every table the P0 subset creates, in FK-dependency order.
const P0_TABLES: &[&str] = &[
    "types_registry__version_family",
    "types_registry__operation",
    "types_registry__operation_item",
    "types_registry__entity",
    "types_registry__type_schema_revision",
    "types_registry__instance_revision",
    "types_registry__type_schema",
    "types_registry__instance",
    "types_registry__dependency",
];

/// Tables that P0 must not create.
const NOT_CREATED_TABLES: &[&str] = &[
    "types_registry__routing_config",
    "types_registry__source_claim",
];

/// Every index `database.sql` declares inside the P0 subset.
const P0_INDEXES: &[&str] = &[
    "idx_tr_operation_status",
    "idx_tr_entity_family",
    "idx_tr_entity_visibility",
    "idx_tr_dependency_to",
];

// The uuid columns hold 16 raw bytes, not the 36-character text form: `sqlx`
// binds `Uuid` as `as_bytes()` on `SQLite`, so the migration declares them
// `BLOB` and guards the width with `ck_tr_*_uuid_len` (see that module's
// header). These are therefore SQL blob literals, interpolated without
// surrounding quotes. The text form would be rejected by that CHECK — which is
// exactly what the CHECK is for. The trailing byte keeps them distinguishable.
const OP_ID: &str = "x'000000000000000000000000000000a1'";
const PRINCIPAL: &str = "x'000000000000000000000000000000b1'";
const ENTITY_UUID: &str = "x'000000000000000000000000000000c1'";
const TENANT: &str = "x'000000000000000000000000000000d1'";
const TS: &str = "2026-08-18T00:00:00Z";
const GTS_TYPE: &str = gts_id!("acme.crm.customer.type.v1~");
const FAMILY_KEY: &str = "gts.acme.crm.customer.type";

fn stmt(db: &DatabaseConnection, sql: impl Into<String>) -> Statement {
    Statement::from_string(db.get_database_backend(), sql.into())
}

/// Fresh in-memory SQLite with the initial migration applied and FK
/// enforcement on — SQLite leaves foreign keys off by default, so the
/// composite-FK test would silently no-op without the PRAGMA.
async fn migrated_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db.execute_raw(stmt(&db, "PRAGMA foreign_keys = ON;"))
        .await
        .expect("enable foreign keys");
    Migrator::up(&db, None)
        .await
        .expect("apply the P0 initial migration");
    db
}

async fn exec(db: &DatabaseConnection, sql: impl Into<String>) -> Result<(), sea_orm::DbErr> {
    db.execute_raw(stmt(db, sql)).await.map(|_| ())
}

/// One global version family; `SQLite` AUTOINCREMENT gives it `id = 1`.
async fn insert_family(db: &DatabaseConnection) {
    exec(
        db,
        format!(
            "INSERT INTO types_registry__version_family \
             (family_key, ownership_scope, owner_tenant_id, created_at) \
             VALUES ('{FAMILY_KEY}', 1, NULL, '{TS}')"
        ),
    )
    .await
    .expect("insert global version family");
}

/// One pending platform-plane registration operation.
async fn insert_operation(db: &DatabaseConnection, id: &str, kind: u8, dry_run: u8) {
    exec(
        db,
        format!(
            "INSERT INTO types_registry__operation \
             (id, kind, dry_run, plane, tenant_id, principal_id, idempotency_key, \
              idempotency_scope_hash, request_fingerprint, status, created_at) \
             VALUES ({id}, {kind}, {dry_run}, 1, NULL, {PRINCIPAL}, 'idem-op', \
                     X'00', X'01', 1, '{TS}')"
        ),
    )
    .await
    .expect("insert operation");
}

// ---------------------------------------------------------------------------
// Shape: the 9 tables, the 4 indexes, and the tables P0 must NOT create.
// ---------------------------------------------------------------------------

/// An installation with only the initial migration still gains coordination state.
#[tokio::test]
async fn an_existing_schema_gains_the_coordination_state_table_and_seed() {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");

    Migrator::up(&db, Some(1))
        .await
        .expect("apply the initial migration alone");
    assert!(
        exec(
            &db,
            "SELECT COUNT(*) FROM types_registry__coordination_state"
        )
        .await
        .is_err(),
        "the initial migration must not create the table, which is the whole point",
    );

    Migrator::up(&db, None).await.expect("apply the rest");

    let seeded = db
        .query_one_raw(stmt(
            &db,
            "SELECT state_seq, updated_at FROM types_registry__coordination_state \
             WHERE state_name = 'entity_write_order'",
        ))
        .await
        .expect("query the seeded state")
        .expect("the upgrade seeds exactly one state row");
    assert_eq!(
        seeded.try_get::<i64>("", "state_seq").expect("state_seq"),
        0,
        "and seeds it at zero",
    );
    assert!(
        seeded.try_get::<String>("", "updated_at").is_ok(),
        "and stamps the seed with a migration timestamp",
    );

    let count = db
        .query_one_raw(stmt(
            &db,
            "SELECT COUNT(*) AS n FROM types_registry__coordination_state",
        ))
        .await
        .expect("count the states")
        .expect("one row");
    assert_eq!(
        count.try_get::<i64>("", "n").expect("count"),
        1,
        "P0 seeds entity_write_order and nothing else; routing belongs to federation",
    );
}

/// The migration preserves a pre-existing table and advanced seed.
#[tokio::test]
async fn the_coordination_state_migration_absorbs_a_table_that_already_exists() {
    /// Timestamp that the migration must preserve.
    const PRESERVED_AT: &str = "2026-09-01T10:00:00.000Z";
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    exec(
        &db,
        "CREATE TABLE types_registry__coordination_state (
             state_name TEXT NOT NULL PRIMARY KEY,
             state_seq INTEGER NOT NULL,
             updated_at TEXT NOT NULL)",
    )
    .await
    .expect("pre-create the table");
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__coordination_state (state_name, state_seq, updated_at) \
             VALUES ('entity_write_order', 7, '{PRESERVED_AT}')"
        ),
    )
    .await
    .expect("pre-seed the row");

    Migrator::up(&db, None)
        .await
        .expect("the migration must absorb the existing table and row");

    let rows = db
        .query_all_raw(stmt(
            &db,
            "SELECT state_seq, updated_at FROM types_registry__coordination_state",
        ))
        .await
        .expect("read the seeded state");
    assert_eq!(rows.len(), 1, "still exactly one row");
    assert_eq!(
        rows[0].try_get::<i64>("", "state_seq").expect("state_seq"),
        7,
        "and the seed must not reset a sequence already in use",
    );
    assert_eq!(
        rows[0]
            .try_get::<String>("", "updated_at")
            .expect("updated_at"),
        PRESERVED_AT,
        "and the seed must not restamp a row that already moved",
    );
}

#[tokio::test]
async fn migration_creates_the_nine_p0_tables() {
    let db = migrated_db().await;
    for table in P0_TABLES {
        exec(&db, format!("SELECT COUNT(*) FROM {table}"))
            .await
            .unwrap_or_else(|e| panic!("table {table} missing after migration: {e}"));
    }
}

// ---------------------------------------------------------------------------
// coordination_state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_second_migration_creates_the_coordination_table_and_seed() {
    let db = migrated_db().await;
    let seeded = db
        .query_one_raw(stmt(
            &db,
            "SELECT state_seq FROM types_registry__coordination_state \
             WHERE state_name = 'entity_write_order'",
        ))
        .await
        .expect("query the seeded state")
        .expect("the seed exists");
    assert_eq!(
        seeded.try_get::<i64>("", "state_seq").expect("state_seq"),
        0
    );
}

#[tokio::test]
async fn coordination_state_rejects_a_negative_sequence() {
    let db = migrated_db().await;
    exec(
        &db,
        "UPDATE types_registry__coordination_state SET state_seq = -1",
    )
    .await
    .expect_err("ck_tr_coordination_state_seq must reject a negative sequence");
}

#[tokio::test]
async fn coordination_state_rejects_a_second_row_for_one_state() {
    let db = migrated_db().await;
    exec(
        &db,
        "INSERT INTO types_registry__coordination_state \
         (state_name, state_seq, updated_at) VALUES ('entity_write_order', 5, \
         '2026-09-04T00:00:00.000Z')",
    )
    .await
    .expect_err("the primary key on state_name must reject a duplicate state");
}

#[tokio::test]
async fn migration_does_not_create_the_federation_tables() {
    let db = migrated_db().await;
    for table in NOT_CREATED_TABLES {
        assert!(
            exec(&db, format!("SELECT COUNT(*) FROM {table}"))
                .await
                .is_err(),
            "{table} must not exist in P0"
        );
    }
}

#[tokio::test]
async fn migration_creates_every_index_declared_in_the_p0_subset() {
    let db = migrated_db().await;
    for index in P0_INDEXES {
        let sql =
            format!("SELECT name FROM sqlite_master WHERE type = 'index' AND name = '{index}'");
        let row = db
            .query_one_raw(stmt(&db, sql))
            .await
            .expect("query sqlite_master");
        assert!(row.is_some(), "index {index} missing after migration");
    }
}

// ---------------------------------------------------------------------------
// version_family
// ---------------------------------------------------------------------------

#[tokio::test]
async fn version_family_owner_check_rejects_global_scope_with_a_tenant() {
    let db = migrated_db().await;
    let err = exec(
        &db,
        format!(
            "INSERT INTO types_registry__version_family \
             (family_key, ownership_scope, owner_tenant_id, created_at) \
             VALUES ('{FAMILY_KEY}', 1, {TENANT}, '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_version_family_owner must reject a global family with a tenant owner");
    assert!(format!("{err}").to_lowercase().contains("constraint"));
}

#[tokio::test]
async fn version_family_owner_check_rejects_tenant_scope_without_a_tenant() {
    let db = migrated_db().await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__version_family \
             (family_key, ownership_scope, owner_tenant_id, created_at) \
             VALUES ('{FAMILY_KEY}', 2, NULL, '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_version_family_owner must reject a tenant family without an owner");
}

#[tokio::test]
async fn version_family_key_is_unique() {
    let db = migrated_db().await;
    insert_family(&db).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__version_family \
             (family_key, ownership_scope, owner_tenant_id, created_at) \
             VALUES ('{FAMILY_KEY}', 1, NULL, '{TS}')"
        ),
    )
    .await
    .expect_err("uq_tr_version_family_key must reject a duplicate family key");
}

// ---------------------------------------------------------------------------
// entity
// ---------------------------------------------------------------------------

/// `ck_tr_entity_owner` — the criterion named in the task: a global row with no
/// `owning_gear` has nothing that answers "who do I ask about this contract".
#[tokio::test]
async fn entity_owner_check_rejects_a_global_row_without_owning_gear() {
    let db = migrated_db().await;
    insert_family(&db).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 1, 1, 1, NULL, NULL, 1, 1, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_entity_owner must reject a global entity with no owning_gear");
}

#[tokio::test]
async fn entity_owner_check_accepts_a_global_row_with_owning_gear() {
    let db = migrated_db().await;
    insert_family(&db).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 1, 1, 1, NULL, 'types-registry', 1, 1, \
                     '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("a global entity naming its owning gear is admissible");
}

#[tokio::test]
async fn entity_lifecycle_check_rejects_a_deleted_row_without_deleted_at() {
    let db = migrated_db().await;
    insert_family(&db).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, deleted_at, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 1, 1, 1, NULL, 'types-registry', 2, 1, \
                     NULL, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_entity_lifecycle must reject a deleted tombstone with no deleted_at");
}

#[tokio::test]
async fn entity_resource_version_check_rejects_zero() {
    let db = migrated_db().await;
    insert_family(&db).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 1, 1, 1, NULL, 'types-registry', 1, 0, \
                     '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("entity resource versions start at 1");
}

#[tokio::test]
async fn entity_family_foreign_key_rejects_an_unknown_family() {
    let db = migrated_db().await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 1, 999, 1, NULL, 'types-registry', 1, 1, \
                     '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("fk_tr_entity_family must reject an entity with no family row");
}

// ---------------------------------------------------------------------------
// operation / operation_item
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operation_plane_check_rejects_the_platform_plane_with_a_tenant() {
    let db = migrated_db().await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation \
             (id, kind, dry_run, plane, tenant_id, principal_id, idempotency_key, \
              idempotency_scope_hash, request_fingerprint, status, created_at) \
             VALUES ({OP_ID}, 1, 0, 1, {TENANT}, {PRINCIPAL}, 'k', X'00', X'01', 1, '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_operation_plane must reject plane = 1 with a tenant_id");
}

#[tokio::test]
async fn operation_state_check_rejects_completed_without_timestamps() {
    let db = migrated_db().await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation \
             (id, kind, dry_run, plane, tenant_id, principal_id, idempotency_key, \
              idempotency_scope_hash, request_fingerprint, status, created_at) \
             VALUES ({OP_ID}, 1, 0, 1, NULL, {PRINCIPAL}, 'k', X'00', X'01', 3, '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_operation_state must reject completed with no started_at/completed_at");
}

#[tokio::test]
async fn operation_dry_run_is_constrained_to_the_boolean_domain() {
    let db = migrated_db().await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation \
             (id, kind, dry_run, plane, tenant_id, principal_id, idempotency_key, \
              idempotency_scope_hash, request_fingerprint, status, created_at) \
             VALUES ({OP_ID}, 1, 7, 1, NULL, {PRINCIPAL}, 'k', X'00', X'01', 1, '{TS}')"
        ),
    )
    .await
    .expect_err("dry_run is a Postgres boolean; the SQLite lowering must reject 7");
}

#[tokio::test]
async fn operation_idempotency_key_is_unique_within_its_scope_hash() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    let other = "x'000000000000000000000000000000a2'";
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation \
             (id, kind, dry_run, plane, tenant_id, principal_id, idempotency_key, \
              idempotency_scope_hash, request_fingerprint, status, created_at) \
             VALUES ({other}, 1, 0, 1, NULL, {PRINCIPAL}, 'idem-op', X'00', X'02', 1, \
                     '{TS}')"
        ),
    )
    .await
    .expect_err("uq_tr_operation_idem must reject a replayed key under the same scope hash");
}

/// `ck_tr_operation_item_state` — the second criterion named in the task.
#[tokio::test]
async fn operation_item_state_check_rejects_succeeded_registration_without_a_revision() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({OP_ID}, 0, '{GTS_TYPE}', 0, 1, 0, 3, NULL, NULL, 1, NULL, \
                     '{TS}', '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err(
        "a committed, changed registration always creates a content revision, so a succeeded \
         non-dry-run registration item with no result_revision_no must be rejected",
    );
}

#[tokio::test]
async fn operation_item_state_check_accepts_succeeded_registration_with_a_revision() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({OP_ID}, 0, '{GTS_TYPE}', 0, 1, 0, 3, NULL, 1, 1, NULL, \
                     '{TS}', '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("a succeeded registration carrying its revision and resource version is admissible");
}

#[tokio::test]
async fn operation_item_state_check_rejects_a_dry_run_success_that_allocated_a_version() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 1).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({OP_ID}, 0, '{GTS_TYPE}', 1, 1, 0, 3, NULL, NULL, 1, NULL, \
                     '{TS}', '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("dry-run `succeeded` writes nothing and allocates no resource version");
}

#[tokio::test]
async fn operation_item_state_check_rejects_unchanged_on_a_first_admission() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({OP_ID}, 0, '{GTS_TYPE}', 0, 1, 0, 4, NULL, NULL, 1, NULL, \
                     '{TS}', '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("`unchanged` is only valid for a registration update with an expected version");
}

/// The composite FK is the only thing that keeps an item's copied `kind` /
/// `dry_run` — which its state CHECK reads — in step with its parent.
#[tokio::test]
async fn operation_item_composite_fk_rejects_a_kind_disagreeing_with_its_parent() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, created_at) \
             VALUES ({OP_ID}, 0, '{GTS_TYPE}', 0, 2, 0, 1, '{{}}', '{TS}')"
        ),
    )
    .await
    .expect_err("fk_tr_operation_item_operation must reject a deletion item under a registration");
}

#[tokio::test]
async fn operation_item_gts_id_is_unique_within_one_operation() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    for item_no in [0, 1] {
        let result = exec(
            &db,
            format!(
                "INSERT INTO types_registry__operation_item \
                 (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, \
                  status, request_payload, created_at) \
                 VALUES ({OP_ID}, {item_no}, '{GTS_TYPE}', 0, 1, 0, 1, '{{}}', '{TS}')"
            ),
        )
        .await;
        if item_no == 0 {
            result.expect("first candidate");
        } else {
            result.expect_err("uq_tr_operation_item_gts must reject a repeated candidate");
        }
    }
}

// ---------------------------------------------------------------------------
// dependency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dependency_kind_check_rejects_an_unknown_kind() {
    let db = migrated_db().await;
    insert_family(&db).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 1, 1, 1, NULL, 'types-registry', 1, 1, \
                     '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("insert entity");
    exec(
        &db,
        "INSERT INTO types_registry__dependency (from_entity_id, kind, to_entity_id) \
         VALUES (1, 4, 1)",
    )
    .await
    .expect_err("ck_tr_dependency_kind admits only 1..=3");
}

// ---------------------------------------------------------------------------
// Roundtrip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn up_down_up_roundtrip_leaves_a_usable_schema() {
    let db = migrated_db().await;
    Migrator::down(&db, None)
        .await
        .expect("roll the migration back");
    for table in P0_TABLES {
        assert!(
            exec(&db, format!("SELECT COUNT(*) FROM {table}"))
                .await
                .is_err(),
            "{table} survived `down`"
        );
    }
    Migrator::up(&db, None)
        .await
        .expect("re-apply the migration");
    insert_family(&db).await;
    for table in P0_TABLES {
        exec(&db, format!("SELECT COUNT(*) FROM {table}"))
            .await
            .unwrap_or_else(|e| panic!("table {table} missing after re-apply: {e}"));
    }
}

// ---------------------------------------------------------------------------
// Gear wiring: the outbox tables come from `toolkit-db`, not from this
// migration. Applying the gear's full set must create both halves; applying
// only the Migrator must create neither outbox table.
// ---------------------------------------------------------------------------

/// A representative outbox table under the gear's configured prefix.
const OUTBOX_TABLE: &str = "types_registry_outbox_outgoing";

#[tokio::test]
async fn the_initial_migration_alone_creates_no_outbox_table() {
    let db = migrated_db().await;
    assert!(
        exec(&db, format!("SELECT COUNT(*) FROM {OUTBOX_TABLE}"))
            .await
            .is_err(),
        "outbox tables are ToolKit-owned and must not be declared by this migration"
    );
}

#[tokio::test]
async fn the_gear_capability_supplies_the_schema_and_the_prefixed_outbox() {
    use sea_orm_migration::MigrationTrait;
    use toolkit::contracts::DatabaseCapability;
    use types_registry::TypesRegistryGear;

    let gear = TypesRegistryGear::default();
    let migrations: Vec<Box<dyn MigrationTrait>> = gear.migrations();
    assert!(
        migrations.len() >= 2,
        "expected the initial migration plus the outbox set, got {}",
        migrations.len()
    );

    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let schema_manager = sea_orm_migration::SchemaManager::new(&db);
    for migration in &migrations {
        migration
            .up(&schema_manager)
            .await
            .expect("apply migration");
    }

    for table in P0_TABLES {
        exec(&db, format!("SELECT COUNT(*) FROM {table}"))
            .await
            .unwrap_or_else(|e| panic!("{table} missing: {e}"));
    }
    exec(&db, format!("SELECT COUNT(*) FROM {OUTBOX_TABLE}"))
        .await
        .expect("the prefixed outbox tables must exist alongside the managed schema");
}

// ---------------------------------------------------------------------------
// Enumeration domains. `database.sql` stores every enumeration as smallint and
// requires its CHECK to enumerate the allowed values rather than accept a
// range. For `kind`, `status` and `entity_kind` that is an explicit `IN` list;
// for `ownership_scope`, `plane` and `lifecycle_status` the branch CHECK does
// it, because no branch matches a third value. Both forms are asserted.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operation_kind_check_rejects_a_value_outside_the_vocabulary() {
    let db = migrated_db().await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation \
             (id, kind, dry_run, plane, tenant_id, principal_id, idempotency_key, \
              idempotency_scope_hash, request_fingerprint, status, created_at) \
             VALUES ({OP_ID}, 3, 0, 1, NULL, {PRINCIPAL}, 'k', X'00', X'01', 1, '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_operation_kind admits only 1 registration and 2 deletion");
}

#[tokio::test]
async fn operation_status_check_rejects_a_value_outside_the_vocabulary() {
    let db = migrated_db().await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation \
             (id, kind, dry_run, plane, tenant_id, principal_id, idempotency_key, \
              idempotency_scope_hash, request_fingerprint, status, created_at) \
             VALUES ({OP_ID}, 1, 0, 1, NULL, {PRINCIPAL}, 'k', X'00', X'01', 9, '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_operation_status admits only 1 pending, 2 running, 3 completed");
}

#[tokio::test]
async fn operation_plane_check_rejects_a_third_plane() {
    let db = migrated_db().await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation \
             (id, kind, dry_run, plane, tenant_id, principal_id, idempotency_key, \
              idempotency_scope_hash, request_fingerprint, status, created_at) \
             VALUES ({OP_ID}, 1, 0, 3, NULL, {PRINCIPAL}, 'k', X'00', X'01', 1, '{TS}')"
        ),
    )
    .await
    .expect_err("no ck_tr_operation_plane branch matches a plane outside {1, 2}");
}

#[tokio::test]
async fn entity_kind_check_rejects_a_third_kind() {
    let db = migrated_db().await;
    insert_family(&db).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 3, 1, 1, NULL, 'types-registry', 1, 1, \
                     '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_entity_kind admits only 1 type_schema and 2 instance");
}

#[tokio::test]
async fn entity_ownership_scope_check_rejects_a_third_scope() {
    let db = migrated_db().await;
    insert_family(&db).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 1, 1, 3, NULL, 'types-registry', 1, 1, \
                     '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("no ck_tr_entity_owner branch matches a scope outside {1, 2}");
}

#[tokio::test]
async fn entity_lifecycle_check_rejects_a_third_status() {
    let db = migrated_db().await;
    insert_family(&db).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 1, 1, 1, NULL, 'types-registry', 3, 1, \
                     '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("P0 lifecycle is only 1 active and 2 deleted");
}

// ---------------------------------------------------------------------------
// operation_item field-shape CHECKs.
// ---------------------------------------------------------------------------

/// One `succeeded` registration item whose numeric fields are all supplied by
/// the caller, so a test can perturb exactly one of them and leave a single
/// CHECK as the only thing that can fire.
async fn insert_succeeded_item(
    db: &DatabaseConnection,
    item_no: i64,
    expected_resource_version: i64,
    result_revision_no: i64,
    result_resource_version: i64,
) -> Result<(), sea_orm::DbErr> {
    exec(
        db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({OP_ID}, {item_no}, '{GTS_TYPE}{item_no}', 0, 1, \
                     {expected_resource_version}, 3, NULL, {result_revision_no}, \
                     {result_resource_version}, NULL, '{TS}', '{TS}', '{TS}')"
        ),
    )
    .await
}

#[tokio::test]
async fn operation_item_no_check_rejects_a_negative_position() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    insert_succeeded_item(&db, -1, 0, 1, 1)
        .await
        .expect_err("ck_tr_operation_item_no requires item_no >= 0");
}

#[tokio::test]
async fn operation_item_precondition_check_rejects_a_negative_expected_version() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    insert_succeeded_item(&db, 0, -1, 1, 1).await.expect_err(
        "ck_tr_operation_item_precondition requires expected_resource_version >= 0, \
         with 0 meaning must_not_exist",
    );
}

#[tokio::test]
async fn operation_item_revision_check_rejects_revision_zero() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    insert_succeeded_item(&db, 0, 0, 0, 1)
        .await
        .expect_err("ck_tr_operation_item_revision requires result_revision_no >= 1");
}

#[tokio::test]
async fn operation_item_resource_version_check_rejects_version_zero() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    insert_succeeded_item(&db, 0, 0, 1, 0)
        .await
        .expect_err("ck_tr_operation_item_resource_version requires >= 1");
}

/// `status = 9` matches no `ck_tr_operation_item_state` branch either, so this
/// pins the shape rather than one named constraint — which is the point: no
/// status outside the vocabulary can be stored.
#[tokio::test]
async fn operation_item_status_outside_the_vocabulary_is_rejected() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, created_at) \
             VALUES ({OP_ID}, 0, '{GTS_TYPE}', 0, 1, 0, 9, '{{}}', '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_operation_item_status admits only 1..=5");
}

#[tokio::test]
async fn operation_item_failed_requires_an_error_payload() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({OP_ID}, 0, '{GTS_TYPE}', 0, 1, 0, 5, NULL, NULL, NULL, NULL, \
                     '{TS}', '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("a failed item distinguishes its cause through error_payload");
}

#[tokio::test]
async fn operation_item_failed_requires_started_at_before_completed_at() {
    let db = migrated_db().await;
    insert_operation(&db, OP_ID, 1, 0).await;
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({OP_ID}, 0, '{GTS_TYPE}', 0, 1, 0, 5, NULL, NULL, NULL, '{{}}', \
                     '{TS}', NULL, '{TS}')"
        ),
    )
    .await
    .expect_err("a failed item cannot complete without first recording started_at");
}

// ---------------------------------------------------------------------------
// The whole nine-table graph, inserted in order. This is what proves the FK
// chain is usable end to end: entity -> operation_item -> type_schema_revision
// -> {type_schema, instance_revision} -> instance, plus a dependency edge.
// ---------------------------------------------------------------------------

const INSTANCE_UUID: &str = "x'000000000000000000000000000000c2'";
const GTS_INSTANCE: &str = "gts.acme.crm.customer.type.v1~acme.default.v1";

#[tokio::test]
async fn every_table_accepts_a_complete_admission_graph() {
    let db = migrated_db().await;
    insert_family(&db).await;
    insert_operation(&db, OP_ID, 1, 0).await;

    for (item_no, gts_id) in [(0, GTS_TYPE), (1, GTS_INSTANCE)] {
        insert_succeeded_item_named(&db, item_no, gts_id)
            .await
            .expect("succeeded registration item");
    }
    for (uuid, gts_id, kind) in [(ENTITY_UUID, GTS_TYPE, 1), (INSTANCE_UUID, GTS_INSTANCE, 2)] {
        exec(
            &db,
            format!(
                "INSERT INTO types_registry__entity \
                 (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
                  owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
                 VALUES ({uuid}, '{gts_id}', {kind}, 1, 1, NULL, 'types-registry', 1, 1, \
                         '{TS}', '{TS}')"
            ),
        )
        .await
        .expect("insert entity");
    }

    exec(
        &db,
        format!(
            "INSERT INTO types_registry__type_schema_revision \
             (entity_id, revision_no, raw_schema, content_hash, gts_spec_version, \
              gts_impl_version, compat_forced, operation_item_id, created_at, updated_at) \
             VALUES (1, 1, '{{}}', X'00', '0.13', '0.12.0', 0, 1, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("insert type schema revision");

    exec(
        &db,
        format!(
            "INSERT INTO types_registry__type_schema \
             (entity_id, revision_no, resolved_schema, effective_traits, \
              effective_traits_schema, resolution_fingerprint, created_at, updated_at) \
             VALUES (1, 1, '{{}}', '{{}}', '{{}}', X'00', '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("insert current type schema");

    exec(
        &db,
        format!(
            "INSERT INTO types_registry__instance_revision \
             (entity_id, revision_no, canonical_value, content_hash, type_schema_entity_id, \
              type_schema_revision_no, gts_spec_version, gts_impl_version, operation_item_id, \
              created_at, updated_at) \
             VALUES (2, 1, '{{}}', X'00', 1, 1, '0.13', '0.12.0', 2, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("insert instance revision");

    exec(
        &db,
        format!(
            "INSERT INTO types_registry__instance (entity_id, revision_no, created_at, updated_at) \
             VALUES (2, 1, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("insert current instance");

    exec(
        &db,
        "INSERT INTO types_registry__dependency (from_entity_id, kind, to_entity_id) \
         VALUES (2, 3, 1)",
    )
    .await
    .expect("insert instance_of dependency edge");

    // fk_tr_type_schema_revision_item is RESTRICT: the revision pins the
    // operation item that admitted it, which is how the admitting principal
    // stays reachable until purge.
    exec(
        &db,
        "DELETE FROM types_registry__operation_item WHERE id = 1",
    )
    .await
    .expect_err("a revision pins its operation item until purge");
}

async fn insert_succeeded_item_named(
    db: &DatabaseConnection,
    item_no: i64,
    gts_id: &str,
) -> Result<(), sea_orm::DbErr> {
    exec(
        db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({OP_ID}, {item_no}, '{gts_id}', 0, 1, 0, 3, NULL, 1, 1, NULL, \
                     '{TS}', '{TS}', '{TS}')"
        ),
    )
    .await
}

#[tokio::test]
async fn type_schema_revision_numbers_start_at_one() {
    let db = migrated_db().await;
    insert_family(&db).await;
    insert_operation(&db, OP_ID, 1, 0).await;
    insert_succeeded_item_named(&db, 0, GTS_TYPE)
        .await
        .expect("succeeded item");
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({ENTITY_UUID}, '{GTS_TYPE}', 1, 1, 1, NULL, 'types-registry', 1, 1, \
                     '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("insert entity");
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__type_schema_revision \
             (entity_id, revision_no, raw_schema, content_hash, gts_spec_version, \
              gts_impl_version, compat_forced, operation_item_id, created_at, updated_at) \
             VALUES (1, 0, '{{}}', X'00', '0.13', '0.12.0', 0, 1, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_type_schema_revision_no requires revision_no >= 1");

    exec(
        &db,
        format!(
            "INSERT INTO types_registry__type_schema_revision \
             (entity_id, revision_no, raw_schema, content_hash, gts_spec_version, \
              gts_impl_version, compat_forced, operation_item_id, created_at, updated_at) \
             VALUES (1, 1, '{{}}', X'00', '0.13', '0.12.0', 7, 1, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("compat_forced is a Postgres boolean; the SQLite lowering must reject 7");
}

/// `ck_tr_operation_item_dry_run_bool` in isolation. An item with `dry_run = 7`
/// also breaks the composite FK to its parent, so foreign keys are switched off
/// for this one database: the CHECK is then the only thing that can reject the
/// row, and the control insert proves the switch-off worked.
#[tokio::test]
async fn operation_item_dry_run_is_constrained_to_the_boolean_domain() {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db.execute_raw(stmt(&db, "PRAGMA foreign_keys = OFF;"))
        .await
        .expect("disable foreign keys");
    Migrator::up(&db, None).await.expect("apply the migration");

    let item = |dry_run: i64| {
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, created_at) \
             VALUES ({OP_ID}, {dry_run}, '{GTS_TYPE}{dry_run}', {dry_run}, 1, 0, 1, '{{}}', \
                     '{TS}')"
        )
    };
    exec(&db, item(0))
        .await
        .expect("control: with foreign keys off, a parentless item inserts");
    exec(&db, item(7))
        .await
        .expect_err("dry_run is a Postgres boolean; the SQLite lowering must reject 7");
}

#[tokio::test]
async fn instance_revision_numbers_start_at_one() {
    let db = migrated_db().await;
    insert_family(&db).await;
    insert_operation(&db, OP_ID, 1, 0).await;
    for (item_no, gts_id) in [(0, GTS_TYPE), (1, GTS_INSTANCE)] {
        insert_succeeded_item_named(&db, item_no, gts_id)
            .await
            .expect("succeeded item");
    }
    for (uuid, gts_id, kind) in [(ENTITY_UUID, GTS_TYPE, 1), (INSTANCE_UUID, GTS_INSTANCE, 2)] {
        exec(
            &db,
            format!(
                "INSERT INTO types_registry__entity \
                 (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
                  owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
                 VALUES ({uuid}, '{gts_id}', {kind}, 1, 1, NULL, 'types-registry', 1, 1, \
                         '{TS}', '{TS}')"
            ),
        )
        .await
        .expect("insert entity");
    }
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__type_schema_revision \
             (entity_id, revision_no, raw_schema, content_hash, gts_spec_version, \
              gts_impl_version, compat_forced, operation_item_id, created_at, updated_at) \
             VALUES (1, 1, '{{}}', X'00', '0.13', '0.12.0', 0, 1, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("insert type schema revision");

    exec(
        &db,
        format!(
            "INSERT INTO types_registry__instance_revision \
             (entity_id, revision_no, canonical_value, content_hash, type_schema_entity_id, \
              type_schema_revision_no, gts_spec_version, gts_impl_version, operation_item_id, \
              created_at, updated_at) \
             VALUES (2, 0, '{{}}', X'00', 1, 1, '0.13', '0.12.0', 2, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_instance_revision_no requires revision_no >= 1");

    // The conforming-schema FK must name a revision that exists.
    exec(
        &db,
        format!(
            "INSERT INTO types_registry__instance_revision \
             (entity_id, revision_no, canonical_value, content_hash, type_schema_entity_id, \
              type_schema_revision_no, gts_spec_version, gts_impl_version, operation_item_id, \
              created_at, updated_at) \
             VALUES (2, 1, '{{}}', X'00', 1, 9, '0.13', '0.12.0', 2, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("fk_tr_instance_revision_schema must name an existing Type Schema revision");
}
