//! Real up/down of the P0 initial migration on `PostgreSQL` and `MySQL`.
//!
//! Covers backend-specific schema behavior unavailable in SQLite, including an
//! idempotent coordination-state seed, then rolls the migration back.
//!
//! `constraint-multi-backend` makes this a correctness requirement: the CHECK
//! expressions lower differently on each engine (`NOT dry_run` over `TINYINT(1)`
//! versus a real `boolean`), so `SQLite`-only evidence does not carry.
//!
//! Gated behind `--features integration` because it needs a Docker daemon:
//!
//! ```text
//! cargo test -p cf-gears-types-registry --features integration --test migration_backends_test
//! ```

#![cfg(feature = "integration")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

use std::time::Duration;

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_gts::gts_id;

use types_registry::infra::storage::Migrator;

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

const OP_ID: &str = "00000000-0000-0000-0000-0000000000a1";
const PRINCIPAL: &str = "00000000-0000-0000-0000-0000000000b1";
const ENTITY_UUID: &str = "00000000-0000-0000-0000-0000000000c1";
const TS: &str = "2026-08-18 00:00:00";
const PRESERVED_AT: OffsetDateTime = datetime!(2026-09-01 10:00:00.123456 UTC);
const GTS_TYPE: &str = gts_id!("acme.crm.customer.type.v1~");
const FAMILY_KEY: &str = "gts.acme.crm.customer.type";

/// Tables intentionally absent in P0.
const NOT_CREATED_TABLES: &[&str] = &[
    "types_registry__routing_config",
    "types_registry__source_claim",
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

async fn exec(db: &DatabaseConnection, sql: impl Into<String>) -> Result<(), sea_orm::DbErr> {
    db.execute_raw(Statement::from_string(
        db.get_database_backend(),
        sql.into(),
    ))
    .await
    .map(|_| ())
}

/// Everything the two backend tests assert once the schema is up: the nine
/// tables answer, `ck_tr_entity_owner` rejects a global entity with no
/// `owning_gear`, and `ck_tr_operation_item_state` rejects a succeeded
/// non-dry-run registration item with no `result_revision_no`.
async fn assert_schema_behaves(db: &DatabaseConnection) {
    let op_id = uuid_literal(db, OP_ID);
    let principal = uuid_literal(db, PRINCIPAL);
    let entity_uuid = uuid_literal(db, ENTITY_UUID);
    for table in P0_TABLES {
        exec(db, format!("SELECT COUNT(*) FROM {table}"))
            .await
            .unwrap_or_else(|e| panic!("{table} missing after migration: {e}"));
    }
    for table in NOT_CREATED_TABLES {
        assert!(
            exec(db, format!("SELECT COUNT(*) FROM {table}"))
                .await
                .is_err(),
            "{table} must not exist in P0"
        );
    }
    assert_coordination_state_behaves(db).await;

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

    // Identity values are backend-assigned, so read the id back rather than
    // assuming 1.
    let row = db
        .query_one_raw(Statement::from_string(
            db.get_database_backend(),
            "SELECT id FROM types_registry__version_family".to_owned(),
        ))
        .await
        .expect("read family id")
        .expect("exactly one family row");
    let family_id: i64 = row.try_get_by_index(0).expect("family id column");

    exec(
        db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({entity_uuid}, '{GTS_TYPE}', 1, {family_id}, 1, NULL, NULL, 1, 1, \
                     '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_entity_owner must reject a global entity with no owning_gear");

    exec(
        db,
        format!(
            "INSERT INTO types_registry__entity \
             (gts_uuid, gts_id, entity_kind, family_id, ownership_scope, owner_tenant_id, \
              owning_gear, lifecycle_status, resource_version, created_at, updated_at) \
             VALUES ({entity_uuid}, '{GTS_TYPE}', 1, {family_id}, 1, NULL, 'types-registry', \
                     1, 1, '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("a global entity naming its owning gear is admissible");

    exec(
        db,
        format!(
            "INSERT INTO types_registry__operation \
             (id, kind, dry_run, plane, tenant_id, principal_id, idempotency_key, \
              idempotency_scope_hash, request_fingerprint, status, created_at) \
             VALUES ({op_id}, 1, FALSE, 1, NULL, {principal}, 'idem-1', \
                     {hash}, {hash}, 1, '{TS}')",
            hash = binary_literal(db)
        ),
    )
    .await
    .expect("insert pending registration operation");

    exec(
        db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({op_id}, 0, '{GTS_TYPE}', FALSE, 1, 0, 3, NULL, NULL, 1, NULL, \
                     '{TS}', '{TS}', '{TS}')"
        ),
    )
    .await
    .expect_err("ck_tr_operation_item_state must reject a succeeded registration with no revision");

    exec(
        db,
        format!(
            "INSERT INTO types_registry__operation_item \
             (operation_id, item_no, gts_id, dry_run, kind, expected_resource_version, status, \
              request_payload, result_revision_no, result_resource_version, error_payload, \
              created_at, started_at, completed_at) \
             VALUES ({op_id}, 0, '{GTS_TYPE}', FALSE, 1, 0, 3, NULL, 1, 1, NULL, \
                     '{TS}', '{TS}', '{TS}')"
        ),
    )
    .await
    .expect("a succeeded registration carrying its revision and resource version is admissible");

    // fk_tr_entity_family is RESTRICT, and Postgres/MySQL both enforce it.
    exec(db, "DELETE FROM types_registry__version_family".to_owned())
        .await
        .expect_err("fk_tr_entity_family RESTRICT must block dropping a family with members");
}

/// Check the seed, backend-specific column types, and constraints.
async fn assert_coordination_state_behaves(db: &DatabaseConnection) {
    let seeded = db
        .query_one_raw(Statement::from_string(
            db.get_database_backend(),
            "SELECT state_seq FROM types_registry__coordination_state \
             WHERE state_name = 'entity_write_order'"
                .to_owned(),
        ))
        .await
        .expect("query the seeded state")
        .expect("the seed exists");
    assert_eq!(
        seeded.try_get::<i64>("", "state_seq").expect("state_seq"),
        0,
        "seeded at zero",
    );

    for expected in expected_column_types(db) {
        let column = expected.column;
        let sql = format!(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 'types_registry__coordination_state' AND column_name = '{column}'"
        );
        let row = db
            .query_one_raw(Statement::from_string(db.get_database_backend(), sql))
            .await
            .expect("query information_schema")
            .unwrap_or_else(|| panic!("{column} present"));
        let data_type: String = row.try_get_by_index(0).expect("data_type");
        assert_eq!(
            normalize_type(&data_type),
            expected.data_type,
            "{column} lowers to {}",
            expected.data_type,
        );

        // Verify declared width or precision from the catalog.
        if let Some((attribute, value)) = expected.attribute {
            let sql = format!(
                "SELECT {attribute} FROM information_schema.columns \
                 WHERE table_name = 'types_registry__coordination_state' \
                 AND column_name = '{column}'"
            );
            let row = db
                .query_one_raw(Statement::from_string(db.get_database_backend(), sql))
                .await
                .expect("query information_schema")
                .unwrap_or_else(|| panic!("{column} present"));
            let actual = catalog_int(db, &row, attribute);
            assert_eq!(
                actual,
                i64::from(value),
                "{column}.{attribute} must be {value}"
            );
        }
    }

    exec(
        db,
        "INSERT INTO types_registry__coordination_state \
         (state_name, state_seq, updated_at) VALUES ('entity_write_order', 5, \
         CURRENT_TIMESTAMP)"
            .to_owned(),
    )
    .await
    .expect_err("the primary key on state_name must reject a duplicate state");

    exec(
        db,
        "UPDATE types_registry__coordination_state SET state_seq = -1".to_owned(),
    )
    .await
    .expect_err("ck_tr_coordination_state_seq must reject a negative sequence");
}

/// Re-run the coordination migration against an advanced live seed.
async fn assert_coordination_seed_is_idempotent(db: &DatabaseConnection) {
    let preserved_at = match db.get_database_backend() {
        sea_orm::DatabaseBackend::Postgres => "'2026-09-01 10:00:00.123456+00'",
        _ => "'2026-09-01 10:00:00.123456'",
    };
    exec(
        db,
        format!(
            "UPDATE types_registry__coordination_state \
             SET state_seq = 7, updated_at = {preserved_at} \
             WHERE state_name = 'entity_write_order'"
        ),
    )
    .await
    .expect("advance the coordination state before re-running its migration");

    let deleted = db
        .execute_raw(Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM seaql_migrations \
             WHERE version = 'm20260904_000002_coordination_state'"
                .to_owned(),
        ))
        .await
        .expect("mark only the coordination-state migration pending");
    assert_eq!(
        deleted.rows_affected(),
        1,
        "the coordination-state migration must have one history row",
    );

    Migrator::up(db, None)
        .await
        .expect("re-run the coordination-state migration against its existing seed");

    let row = db
        .query_one_raw(Statement::from_string(
            db.get_database_backend(),
            "SELECT state_seq, updated_at FROM types_registry__coordination_state \
             WHERE state_name = 'entity_write_order'"
                .to_owned(),
        ))
        .await
        .expect("read the preserved state")
        .expect("the state row remains present");
    assert_eq!(
        row.try_get::<i64>("", "state_seq").expect("state_seq"),
        7,
        "re-running the seed must not reset an advanced sequence",
    );
    assert_eq!(
        row.try_get::<OffsetDateTime>("", "updated_at")
            .expect("updated_at"),
        PRESERVED_AT,
        "re-running the seed must not restamp the row",
    );
}

/// Expected backend catalog representation of a column.
struct ColumnExpectation {
    column: &'static str,
    data_type: &'static str,
    /// Optional catalog width or precision.
    attribute: Option<(&'static str, i32)>,
}

fn expected_column_types(db: &DatabaseConnection) -> [ColumnExpectation; 3] {
    match db.get_database_backend() {
        // `varchar(64)` and `timestamptz`, spelled out by the catalog.
        sea_orm::DatabaseBackend::Postgres => [
            ColumnExpectation {
                column: "state_name",
                data_type: "character varying",
                attribute: Some(("character_maximum_length", 64)),
            },
            ColumnExpectation {
                column: "state_seq",
                data_type: "bigint",
                attribute: None,
            },
            ColumnExpectation {
                column: "updated_at",
                data_type: "timestamp with time zone",
                attribute: None,
            },
        ],
        // MySQL reports width and precision in separate columns.
        _ => [
            ColumnExpectation {
                column: "state_name",
                data_type: "varchar",
                attribute: Some(("character_maximum_length", 64)),
            },
            ColumnExpectation {
                column: "state_seq",
                data_type: "bigint",
                attribute: None,
            },
            ColumnExpectation {
                column: "updated_at",
                data_type: "datetime",
                attribute: Some(("datetime_precision", 6)),
            },
        ],
    }
}

/// Decode a catalog integer using the backend's reported type.
fn catalog_int(db: &DatabaseConnection, row: &sea_orm::QueryResult, attribute: &str) -> i64 {
    match db.get_database_backend() {
        sea_orm::DatabaseBackend::MySql if attribute == "character_maximum_length" => {
            row.try_get_by_index::<i64>(0).expect(attribute)
        }
        sea_orm::DatabaseBackend::MySql => {
            let v: u32 = row.try_get_by_index(0).expect(attribute);
            i64::from(v)
        }
        _ => {
            let v: i32 = row.try_get_by_index(0).expect(attribute);
            i64::from(v)
        }
    }
}

/// Normalize backend catalog type names for comparison.
fn normalize_type(data_type: &str) -> String {
    data_type.trim().to_lowercase()
}

/// `uuid` takes different literal syntax as well. Postgres has a native `uuid`
/// type that accepts the 36-character text form; `MySQL` stores the 16 raw bytes
/// `sqlx` binds in `BINARY(16)` and so needs a hex literal. See the migration
/// module's header for why the storage differs.
fn uuid_literal(db: &DatabaseConnection, text: &str) -> String {
    match db.get_database_backend() {
        sea_orm::DatabaseBackend::Postgres => format!("'{text}'"),
        _ => format!("x'{}'", text.replace('-', "")),
    }
}

/// `bytea` and `VARBINARY` take different literal syntax.
fn binary_literal(db: &DatabaseConnection) -> &'static str {
    match db.get_database_backend() {
        sea_orm::DatabaseBackend::Postgres => "'\\x00'::bytea",
        _ => "X'00'",
    }
}

#[tokio::test]
async fn migration_applies_and_rolls_back_on_postgres() {
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

    let db = Database::connect(format!("postgres://user:pass@{host}:{port}/app"))
        .await
        .expect("connect postgres");

    Migrator::up(&db, None).await.expect("apply on postgres");
    assert_schema_behaves(&db).await;
    assert_coordination_seed_is_idempotent(&db).await;
    Migrator::down(&db, None)
        .await
        .expect("roll back on postgres");
    for table in P0_TABLES {
        assert!(
            exec(&db, format!("SELECT COUNT(*) FROM {table}"))
                .await
                .is_err(),
            "{table} survived `down` on postgres"
        );
    }
    Migrator::up(&db, None).await.expect("re-apply on postgres");
}

#[tokio::test]
async fn migration_applies_and_rolls_back_on_mysql() {
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

    let db = Database::connect(format!("mysql://root@{host}:{port}/test"))
        .await
        .expect("connect mysql");

    Migrator::up(&db, None).await.expect("apply on mysql");
    assert_schema_behaves(&db).await;
    assert_coordination_seed_is_idempotent(&db).await;
    Migrator::down(&db, None).await.expect("roll back on mysql");
    for table in P0_TABLES {
        assert!(
            exec(&db, format!("SELECT COUNT(*) FROM {table}"))
                .await
                .is_err(),
            "{table} survived `down` on mysql"
        );
    }
    Migrator::up(&db, None).await.expect("re-apply on mysql");
}
