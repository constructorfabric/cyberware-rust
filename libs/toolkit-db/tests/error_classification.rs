#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(all(
    feature = "integration",
    any(feature = "sqlite", feature = "pg", feature = "mysql")
))]

//! Regression tests for the two DB-error classifiers that inspect error *text*.
//!
//! [`toolkit_db::secure::is_unique_violation`] and
//! [`toolkit_db::contention::is_retryable_contention`] both fall back to
//! matching substrings of `DbErr`'s `Display` output (SQLSTATE codes, driver
//! message fragments). That makes them uniquely fragile across a `SeaORM` / sqlx
//! upgrade: the code keeps compiling while the classification silently starts
//! returning `false`, and the visible symptom is only "retries stopped
//! happening" or "a duplicate insert surfaces as a generic 500".
//!
//! The unit tests next to those functions synthesise a `DbErr` with a
//! hand-written message, so they cannot catch that drift. These tests provoke
//! **real** errors from a **real** engine and assert the classifier still
//! recognises them, on every backend we support.
//!
//! Added during the `SeaORM` 1.1 -> 2.0 / sqlx 0.8 -> 0.9 upgrade (#4543).

mod common;

use anyhow::Result;
use sea_orm::Set;
use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude as mig;
use sea_orm_migration::prelude::Iden;
use sea_orm_migration::sea_query;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{ScopableEntity, ScopeError, secure_insert};
use toolkit_db::{DbConnConfig, build_db};
use toolkit_security::{AccessScope, pep_properties};
use uuid::Uuid;

#[derive(Iden)]
enum ClassifyTbl {
    #[iden = "error_classify"]
    Table,
    Id,
    TenantId,
}

struct CreateClassifyTable;

impl mig::MigrationName for CreateClassifyTable {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "m001_create_error_classify"
    }
}

#[async_trait::async_trait]
impl mig::MigrationTrait for CreateClassifyTable {
    async fn up(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .create_table(
                mig::Table::create()
                    .table(ClassifyTbl::Table)
                    .if_not_exists()
                    .col(
                        mig::ColumnDef::new(ClassifyTbl::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(mig::ColumnDef::new(ClassifyTbl::TenantId).uuid().not_null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .drop_table(mig::Table::drop().table(ClassifyTbl::Table).to_owned())
            .await
    }
}

mod ent {
    use sea_orm::entity::prelude::*;
    use uuid::Uuid;

    #[derive(Debug, Clone, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "error_classify")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

impl ScopableEntity for ent::Entity {
    fn tenant_col() -> Option<<Self as EntityTrait>::Column> {
        Some(ent::Column::TenantId)
    }
    fn resource_col() -> Option<<Self as EntityTrait>::Column> {
        Some(ent::Column::Id)
    }
    fn owner_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn type_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn resolve_property(property: &str) -> Option<<Self as EntityTrait>::Column> {
        match property {
            p if p == pep_properties::OWNER_TENANT_ID => Self::tenant_col(),
            p if p == pep_properties::RESOURCE_ID => Self::resource_col(),
            _ => None,
        }
    }
    fn scope_columns() -> Vec<<Self as EntityTrait>::Column> {
        vec![ent::Column::TenantId, ent::Column::Id]
    }
}

/// Insert the same primary key twice and assert the resulting error is still
/// recognised as a unique-constraint violation.
///
/// This exercises both halves of [`is_unique_violation`]: `SeaORM`'s own
/// `sql_err()` classification and, if that returns `None`, the message-substring
/// fallback. Either is fine — what must not happen is *neither* matching.
async fn assert_duplicate_insert_is_unique_violation(db: toolkit_db::Db) -> Result<()> {
    run_migrations_for_testing(&db, vec![Box::new(CreateClassifyTable)])
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let tenant_id = Uuid::new_v4();
    let scope = AccessScope::for_tenants(vec![tenant_id]);
    let id = Uuid::new_v4();

    let conn = db.conn().expect("conn");

    let am = ent::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
    };
    secure_insert::<ent::Entity>(am, &scope, &conn)
        .await
        .map_err(|e| anyhow::anyhow!("first insert should succeed: {e}"))?;

    // Same primary key -> real PK/unique violation from the engine.
    let dup = ent::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
    };
    let err = secure_insert::<ent::Entity>(dup, &scope, &conn)
        .await
        .expect_err("duplicate primary key must be rejected by the database");

    assert!(
        err.is_unique_violation(),
        "is_unique_violation() must still recognise a real duplicate-key error \
         after the SeaORM/sqlx upgrade; classifier saw: {err}"
    );

    // The structured accessor must reach this error too, on every backend the
    // workspace supports. What the code *means* differs per engine and is
    // asserted where that difference matters (`db_error`'s SQLite test, and
    // the PostgreSQL RESTRICT test below); what must hold everywhere is that a
    // real driver refusal is reachable at all, since a gear classifying
    // without `sqlx` has nothing else to read.
    let ScopeError::Db(db_err) = &err else {
        panic!("a duplicate insert must surface the database error: {err}");
    };
    let refusal = toolkit_db::db_error::driver_refusal(db_err)
        .unwrap_or_else(|| panic!("driver_refusal must reach a real refusal: {err}"));
    assert!(
        !refusal.code().is_empty(),
        "the driver must have reported a code: {refusal:?}"
    );
    // `DbBackend` has no `Display`; the name is enough to tell the lanes apart
    // in a run's output.
    let backend = match db.backend() {
        sea_orm::DatabaseBackend::Postgres => "postgres",
        sea_orm::DatabaseBackend::MySql => "mysql",
        sea_orm::DatabaseBackend::Sqlite => "sqlite",
        // `DatabaseBackend` is non-exhaustive.
        _ => "unknown backend",
    };
    println!(
        "{backend} reported code {} for the duplicate key",
        refusal.code()
    );

    Ok(())
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_duplicate_insert_is_classified_as_unique_violation() -> Result<()> {
    let config = DbConnConfig {
        dsn: Some(toolkit_utils::SecretString::new("sqlite::memory:")),
        ..Default::default()
    };
    let db = build_db(config, None).await?;
    assert_duplicate_insert_is_unique_violation(db).await
}

#[cfg(feature = "pg")]
#[tokio::test]
async fn pg_duplicate_insert_is_classified_as_unique_violation() -> Result<()> {
    let dut = common::bring_up_postgres().await?;
    let config = DbConnConfig {
        dsn: Some(toolkit_utils::SecretString::new(dut.url)),
        ..Default::default()
    };
    let db = build_db(config, None).await?;
    assert_duplicate_insert_is_unique_violation(db).await
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql_duplicate_insert_is_classified_as_unique_violation() -> Result<()> {
    let dut = common::bring_up_mysql().await?;
    let config = DbConnConfig {
        dsn: Some(toolkit_utils::SecretString::new(dut.url)),
        ..Default::default()
    };
    let db = build_db(config, None).await?;
    assert_duplicate_insert_is_unique_violation(db).await
}

/// Provoke a real `SQLITE_BUSY` and assert `is_retryable_contention` still says
/// "retry me".
///
/// Two write transactions against the same file-backed database with
/// `busy_timeout=0`: the first takes the write lock, the second is refused
/// immediately rather than waiting. `:memory:` is unusable here because each
/// connection would get its own private database.
///
/// The two transactions are interleaved with channels because `Db::transaction`
/// owns the commit — the holder's closure parks until the blocked writer has had
/// its attempt. Everything goes through toolkit-db's public API; `DbConn`/`DbTx`
/// intentionally do not expose raw `SeaORM` transaction handles to callers.
///
/// This is the classifier that fails *silently* — if the `SQLite` driver's message
/// text stops containing `(code: 5)` / `database is locked`, retries quietly
/// stop happening under contention.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_busy_is_classified_as_retryable_contention() -> Result<()> {
    use sea_orm::DbBackend;
    use tokio::sync::oneshot;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("busy.sqlite");
    let dsn = format!("sqlite://{}?mode=rwc", path.display());

    let mut params = std::collections::HashMap::new();
    // Refuse instead of waiting, so the test is deterministic and fast.
    params.insert("busy_timeout".to_owned(), "0".to_owned());
    // Rollback-journal mode: WAL lets a reader and a writer coexist, which makes
    // write-lock contention much harder to force.
    params.insert("journal_mode".to_owned(), "DELETE".to_owned());

    let build = |dsn: String, params: std::collections::HashMap<String, String>| async move {
        build_db(
            DbConnConfig {
                dsn: Some(toolkit_utils::SecretString::new(dsn)),
                params: Some(params),
                ..Default::default()
            },
            None,
        )
        .await
    };

    let db_a = build(dsn.clone(), params.clone()).await?;
    run_migrations_for_testing(&db_a, vec![Box::new(CreateClassifyTable)])
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let db_b = build(dsn, params).await?;

    let tenant_id = Uuid::new_v4();
    let scope = AccessScope::for_tenants(vec![tenant_id]);

    let (lock_held_tx, lock_held_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = oneshot::channel::<()>();

    // Holder: write (taking the SQLite write lock), announce, then park until the
    // blocked writer has had its turn. Committing early would release the lock.
    let holder_scope = scope.clone();
    let holder = tokio::spawn(async move {
        let (_db, res) = db_a
            .transaction(move |tx| {
                let scope = holder_scope.clone();
                Box::pin(async move {
                    secure_insert::<ent::Entity>(
                        ent::ActiveModel {
                            id: Set(Uuid::new_v4()),
                            tenant_id: Set(tenant_id),
                        },
                        &scope,
                        tx,
                    )
                    .await?;
                    lock_held_tx.send(()).ok();
                    release_rx.await.ok();
                    Ok::<(), anyhow::Error>(())
                })
            })
            .await;
        res
    });

    lock_held_rx
        .await
        .map_err(|_| anyhow::anyhow!("holder transaction failed before taking the write lock"))?;

    let conn_b = db_b.conn().expect("conn b");
    let err = secure_insert::<ent::Entity>(
        ent::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
        },
        &scope,
        &conn_b,
    )
    .await
    .expect_err("second writer must be refused while the write lock is held");

    release_tx.send(()).ok();
    holder.await??;

    let toolkit_db::secure::ScopeError::Db(db_err) = &err else {
        anyhow::bail!("expected a database error from the blocked writer, got: {err}");
    };
    assert!(
        toolkit_db::contention::is_retryable_contention(DbBackend::Sqlite, db_err),
        "is_retryable_contention() must still recognise a real SQLITE_BUSY after \
         the SeaORM/sqlx upgrade, otherwise transaction retries silently stop \
         happening under contention; classifier saw: {db_err}"
    );

    Ok(())
}

/// Provoke a real `PostgreSQL` serialization failure (SQLSTATE 40001) between two
/// `SERIALIZABLE` transactions and assert `is_retryable_contention` still says
/// "retry me".
///
/// Classic write skew: both transactions read the whole table (fixing their
/// snapshots), then both insert. `PostgreSQL`'s predicate-locking detector can
/// abort *either* side once it sees the conflict — not necessarily "whichever
/// commits second" — so this races both transactions and asserts on whichever
/// one actually failed, rather than assuming which one it will be. This is the
/// exact condition `Db::transaction_with_retry` absorbs, and it only works
/// because that retry helper asks `is_retryable_contention`.
#[cfg(feature = "pg")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_serialization_failure_is_classified_as_retryable_contention() -> Result<()> {
    use sea_orm::DbBackend;
    use tokio::sync::oneshot;
    use toolkit_db::secure::SecureEntityExt;

    let dut = common::bring_up_postgres().await?;
    let cfg = || DbConnConfig {
        dsn: Some(toolkit_utils::SecretString::new(dut.url.clone())),
        ..Default::default()
    };

    let db_a = build_db(cfg(), None).await?;
    run_migrations_for_testing(&db_a, vec![Box::new(CreateClassifyTable)])
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let db_b = build_db(cfg(), None).await?;

    let tenant_id = Uuid::new_v4();
    let scope = AccessScope::for_tenants(vec![tenant_id]);
    let serializable = || toolkit_db::secure::TxConfig {
        isolation: Some(toolkit_db::secure::TxIsolationLevel::Serializable),
        access_mode: None,
    };

    let (a_read_tx, a_read_rx) = oneshot::channel::<()>();
    let (b_read_tx, b_read_rx) = oneshot::channel::<()>();

    // A: fix the snapshot with a read, wait for B to do the same, then insert.
    // Both transactions committing after both have read-then-written is what
    // creates the write-skew conflict; which side PostgreSQL aborts to break it
    // is not guaranteed to be "whichever commits second".
    let scope_a = scope.clone();
    let a = tokio::spawn(async move {
        let (_db, res) = db_a
            .transaction_with_config(serializable(), move |tx| {
                let scope = scope_a.clone();
                Box::pin(async move {
                    let _ = ent::Entity::find()
                        .secure()
                        .scope_with(&scope)
                        .count(tx)
                        .await?;
                    a_read_tx.send(()).ok();
                    b_read_rx.await.ok();
                    secure_insert::<ent::Entity>(
                        ent::ActiveModel {
                            id: Set(Uuid::new_v4()),
                            tenant_id: Set(tenant_id),
                        },
                        &scope,
                        tx,
                    )
                    .await?;
                    Ok::<(), anyhow::Error>(())
                })
            })
            .await;
        res
    });

    let (_db_b, res_b) = db_b
        .transaction_with_config(serializable(), move |tx| {
            let scope = scope.clone();
            Box::pin(async move {
                let _ = ent::Entity::find()
                    .secure()
                    .scope_with(&scope)
                    .count(tx)
                    .await?;
                b_read_tx.send(()).ok();
                a_read_rx.await.ok();
                secure_insert::<ent::Entity>(
                    ent::ActiveModel {
                        id: Set(Uuid::new_v4()),
                        tenant_id: Set(tenant_id),
                    },
                    &scope,
                    tx,
                )
                .await?;
                Ok::<(), anyhow::Error>(())
            })
        })
        .await;

    let res_a = a.await?;

    // Exactly one side should lose the write-skew conflict; classify whichever
    // one PostgreSQL aborted, not a side picked in advance.
    let failed = match (res_a, res_b) {
        (Err(e), Ok(())) | (Ok(()), Err(e)) => e,
        (Ok(()), Ok(())) => {
            anyhow::bail!(
                "expected PostgreSQL to abort one side of the write-skew conflict, but both committed"
            )
        }
        (Err(e_a), Err(e_b)) => {
            anyhow::bail!("expected exactly one side to fail, but both did: a={e_a}, b={e_b}")
        }
    };

    // Whichever transaction lost the conflict may have failed on the plain
    // `.count()` read (a bare `sea_orm::DbErr`) or on `secure_insert` (which
    // wraps it as `ScopeError::Db(DbErr)`). `ScopeError::Db`'s `#[from]` gives
    // it a `source()` pointing at the `DbErr`, so walk the chain rather than
    // assuming which shape came back.
    let db_err = failed
        .chain()
        .find_map(|e| e.downcast_ref::<sea_orm::DbErr>())
        .ok_or_else(|| {
            anyhow::anyhow!("expected a sea_orm::DbErr in the error chain, got: {failed}")
        })?;

    assert!(
        toolkit_db::contention::is_retryable_contention(DbBackend::Postgres, db_err),
        "is_retryable_contention() must still recognise a real PostgreSQL \
         serialization failure after the SeaORM/sqlx upgrade, otherwise \
         transaction retries silently stop happening; classifier saw: {db_err}"
    );

    Ok(())
}

// ════════════════════════════════════════════════════════════════════
// FK / RESTRICT, from a real server
// ════════════════════════════════════════════════════════════════════

/// Delete a row a `RESTRICT` foreign key still references, on a real server,
/// and assert what comes back is classified as a foreign-key refusal.
///
/// `RESTRICT` rather than `NO ACTION` on purpose: it is the action whose
/// SQLSTATE `PostgreSQL` 18 changed, and the two are not interchangeable —
/// `RESTRICT` is checked immediately and cannot be deferred.
///
/// The point of a live server rather than a `DbErr` built from a literal: the
/// code and the wording are the server's to change, and it changed both.
/// `PostgreSQL` 18 reports this refusal as `23001` (`restrict_violation`)
/// where 17 and earlier reported `23503`, and reworded the message from
/// "violates foreign key constraint" to "violates RESTRICT setting of foreign
/// key constraint". A test shaped `classify("23503") == ForeignKey` cannot
/// notice either, because the code under test and the test agree on a premise
/// the server has abandoned (issue #4645).
///
/// Three assertions, none of them pinned to one major:
///
/// 1. the classifier says foreign-key refusal;
/// 2. the structured accessor reaches a SQLSTATE, and it is one of the two
///    codes that name this condition — printed, so a run records what this
///    server actually sent;
/// 3. the driver named the constraint, which is what tells two foreign keys on
///    one table apart.
#[cfg(feature = "pg")]
#[tokio::test]
async fn pg_restrict_delete_is_classified_as_foreign_key_violation() -> Result<()> {
    use sea_orm::ConnectionTrait as _;
    use toolkit_db::db_error::{ConstraintViolation, driver_refusal};
    use toolkit_db::secure::is_foreign_key_violation;

    let dut = common::bring_up_postgres().await?;
    // A plain connection: the subject is how a driver refusal classifies, not
    // how toolkit-db wraps a pool, and raw DDL is what sets the FK up.
    let conn = sea_orm::Database::connect(dut.url.clone()).await?;

    conn.execute_unprepared(
        "CREATE TABLE restrict_parent (id uuid PRIMARY KEY); \
         CREATE TABLE restrict_child ( \
             id uuid PRIMARY KEY, \
             parent_id uuid NOT NULL, \
             CONSTRAINT restrict_child_parent_fk \
                 FOREIGN KEY (parent_id) REFERENCES restrict_parent (id) \
                 ON DELETE RESTRICT \
         );",
    )
    .await?;

    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    conn.execute_unprepared(&format!(
        "INSERT INTO restrict_parent (id) VALUES ('{parent}'); \
         INSERT INTO restrict_child (id, parent_id) VALUES ('{child}', '{parent}');"
    ))
    .await?;

    let err = conn
        .execute_unprepared(&format!(
            "DELETE FROM restrict_parent WHERE id = '{parent}'"
        ))
        .await
        .expect_err("a RESTRICT foreign key must refuse this delete");

    assert!(
        is_foreign_key_violation(&err),
        "a live RESTRICT refusal must classify as a foreign-key violation: {err}"
    );

    let refusal = driver_refusal(&err).unwrap_or_else(|| {
        panic!("the structured accessor must reach the driver's refusal: {err}")
    });
    println!(
        "PostgreSQL reported SQLSTATE {} for the RESTRICT refusal",
        refusal.code()
    );
    assert!(
        matches!(refusal.code(), "23001" | "23503"),
        "an FK refusal must arrive as one of the two codes that name it, got {}: {err}",
        refusal.code()
    );
    assert_eq!(
        refusal.violation(),
        Some(ConstraintViolation::ForeignKey),
        "both codes must name one condition"
    );
    assert_eq!(
        refusal.constraint(),
        Some("restrict_child_parent_fk"),
        "the driver must name the constraint that refused"
    );

    Ok(())
}
