//! End-to-end `SQLite` smoke test for the rbac persistence stack.
//!
//! Opens an in-memory `SQLite` database, runs the rbac migrator, and
//! drives the two `SeaORM` repository structs through a tiny lifecycle —
//! create → get → list → create-assignment → delete-assignment →
//! delete-role. The goal is to keep the `SQLite` path of the dual-driver
//! migration honest; broader behaviour is covered by the `Postgres`
//! integration tests under `tests/postgres_*.rs`.
//!
//! Unlike those tests this one does **not** need Docker, so it runs as
//! part of the default `cargo test -p cf-gears-rbac` invocation.

#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use anyhow::Result;
use rbac::domain::etag::etag_for;
use rbac::domain::role_assignment_repo::{
    NewRoleAssignment, RoleAssignmentRepository as RoleAssignmentRepoTrait,
};
use rbac::domain::role_definition_repo::{
    NewRoleDefinition, RoleDefinitionRepository as RoleDefinitionRepoTrait,
};
use rbac::infra::storage::migrations::Migrator;
use rbac::infra::storage::repo::role_assignment_repo::RoleAssignmentRepository;
use rbac::infra::storage::repo::role_definition_repo::RoleDefinitionRepository;
use rbac_sdk::models::{PermissionRule, PrincipalType, Scope};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// Open a fresh in-memory `SQLite` database and run the rbac migrations.
/// Each call returns its own `DBProvider` because `SeaORM` holds onto the
/// underlying pool — sharing between tests would race on `CREATE TABLE`.
async fn fresh_sqlite_provider() -> Result<DBProvider<DbError>> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default()).await?;
    run_migrations_for_testing(&db, Migrator::migrations()).await?;
    Ok(DBProvider::new(db))
}

#[tokio::test]
async fn role_definition_lifecycle_round_trips_on_sqlite() -> Result<()> {
    let provider = fresh_sqlite_provider().await?;
    // The repository is stateless now; the executor is passed per call.
    let conn = provider.conn()?;
    let repo = RoleDefinitionRepository;

    let tenant = Uuid::now_v7();
    let new = NewRoleDefinition {
        id: Uuid::now_v7(),
        name: "SqliteSmoke-Reader".to_owned(),
        description: Some("smoke test".to_owned()),
        permissions: vec![PermissionRule::new(
            "read",
            "gts.cf.resources.compute.vm.v1~",
        )],
        not_permissions: vec![PermissionRule::new(
            "write",
            "gts.cf.resources.compute.vm.v1~",
        )],
        assignable_scopes: vec![Scope::tenant(tenant)],
        owner_tenant_id: tenant,
        created_by: "tester".to_owned(),
    };
    let created = repo
        .create(&conn, new)
        .await
        .expect("SQLite create must succeed");
    assert_eq!(created.name, "SqliteSmoke-Reader");
    assert_eq!(created.permissions.len(), 1);
    assert_eq!(created.not_permissions.len(), 1);
    assert_eq!(created.permissions[0].operation, "read");
    assert_eq!(created.not_permissions[0].operation, "write");

    let fetched = repo
        .find_by_id(&conn, created.id)
        .await
        .expect("find_by_id must succeed")
        .expect("row MUST be present after create");
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.permissions, created.permissions);
    assert_eq!(fetched.not_permissions, created.not_permissions);

    Ok(())
}

#[tokio::test]
async fn role_assignment_create_and_delete_round_trips_on_sqlite() -> Result<()> {
    let provider = fresh_sqlite_provider().await?;
    let conn = provider.conn()?;
    let role_repo = RoleDefinitionRepository;
    let assignment_repo = RoleAssignmentRepository;

    let tenant = Uuid::now_v7();
    let role = role_repo
        .create(
            &conn,
            NewRoleDefinition {
                id: Uuid::now_v7(),
                name: "SqliteSmoke-Auditor".to_owned(),
                description: None,
                permissions: vec![PermissionRule::new(
                    "read",
                    "gts.cf.resources.compute.vm.v1~",
                )],
                not_permissions: Vec::new(),
                assignable_scopes: vec![Scope::tenant(tenant)],
                owner_tenant_id: tenant,
                created_by: "tester".to_owned(),
            },
        )
        .await
        .expect("seed role definition");

    let assignment = assignment_repo
        .create(
            &conn,
            NewRoleAssignment {
                role_definition_id: role.id,
                principal_id: "subject-1".to_owned(),
                principal_type: PrincipalType::User,
                scope: rbac_sdk::models::Scope::tenant(tenant),
                created_by: "tester".to_owned(),
                // The author identity is a display-path concern; these
                // fixtures seed rows directly and record none.
                created_by_type: None,
                created_by_tenant_id: None,
            },
        )
        .await
        .expect("create assignment");

    assert_eq!(assignment.role_definition_id, role.id);
    assert_eq!(assignment.principal_id, "subject-1");

    assignment_repo
        .delete(&conn, assignment.id)
        .await
        .expect("delete assignment");

    // Role definition can now be deleted because no assignments remain.
    let role_etag = etag_for(role.updated_at, role.id);
    role_repo
        .delete(&conn, role.id, &role_etag)
        .await
        .expect("delete role");

    Ok(())
}

#[tokio::test]
async fn duplicate_name_per_tenant_maps_to_name_taken_on_sqlite() -> Result<()> {
    use rbac::domain::error::DomainError;

    // Exercises the SQLite column-set fallback in `matches_constraint`:
    // SQLite's error message lacks the structured constraint name, so
    // the repo must recognise the violation by the columns mentioned
    // (`name, owner_tenant_id`).
    let provider = fresh_sqlite_provider().await?;
    // The repository is stateless now; the executor is passed per call.
    let conn = provider.conn()?;
    let repo = RoleDefinitionRepository;

    let tenant = Uuid::now_v7();
    let mk = |suffix: u8| NewRoleDefinition {
        id: Uuid::now_v7(),
        name: "DupName".to_owned(),
        description: Some(format!("attempt {suffix}")),
        permissions: vec![PermissionRule::new(
            "read",
            "gts.cf.resources.compute.vm.v1~",
        )],
        not_permissions: Vec::new(),
        assignable_scopes: vec![Scope::tenant(tenant)],
        owner_tenant_id: tenant,
        created_by: "tester".to_owned(),
    };

    repo.create(&conn, mk(1)).await.expect("first create");
    let err = repo
        .create(&conn, mk(2))
        .await
        .expect_err("duplicate MUST reject");
    assert!(
        matches!(err, DomainError::RoleDefinitionNameTaken { .. }),
        "expected RoleDefinitionNameTaken, got {err:?}"
    );
    Ok(())
}
