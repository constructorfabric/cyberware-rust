//! SQLite sibling of `postgres_role_assignment_repo.rs` for the
//! **tenant/scope isolation** guarantees.
//!
//! Those guarantees — `scope_prefix` returning descendants only,
//! `VisibilityFilter` narrowing to the caller's subtrees, and
//! `get_subject_assignments` excluding an ancestor tenant's resource
//! groups — were asserted only in Docker-gated `#[ignore]`d files. The
//! sole non-ignored repository coverage was `sqlite_smoke.rs`, which
//! asserts round-trips and a duplicate-name mapping and nothing about
//! scope filtering, so a regression that widened the allow-set produced
//! a clean default `cargo test` run.
//!
//! None of the assertions below depend on a PostgreSQL dialect feature:
//! they exercise `scope` / `tenant_id` / `scope_depth` column predicates
//! and `LIKE` prefixes, which SeaORM emits identically on both backends.
//! Genuinely PG-specific behaviour (SQLSTATE mappings, PL/pgSQL
//! triggers, `FOR UPDATE`, concurrent writers) stays in the Postgres
//! file.

#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod common;

use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use rbac::domain::role_assignment_repo::{
    NewRoleAssignment, RoleAssignmentRepository, SubjectAssignmentsQuery, VisibilityFilter,
};
use rbac::domain::role_definition_repo::{NewRoleDefinition, RoleDefinitionRepository};
use rbac::infra::storage::{role_assignment_repo, role_definition_repo};
use rbac_sdk::models::PrincipalType;

/// Fixed tenant/RG ids so the expected-scope vectors below read as
/// literals, exactly as they do in the Postgres file.
const T1: &str = "11111111-1111-1111-1111-111111111111";
const T2: &str = "22222222-2222-2222-2222-222222222222";
/// Shares the `/tenants/0000...01` textual prefix with nothing under T1,
/// but is the classic candidate for a `LIKE` predicate built without the
/// trailing separator.
const T_SIBLING_PREFIX: &str = "00000000-0000-0000-0000-000000000010";
const RG1: &str = "11111111-aaaa-bbbb-cccc-111111111111";

/// Build an `ODataQuery` for tests. Mirrors the Postgres file's helper:
/// `filter_str` is parsed through `toolkit_odata::parse_filter_string`
/// so the tests exercise the real filter pipeline rather than a
/// hand-built AST.
fn build_query(filter_str: Option<&str>, limit: Option<u64>) -> toolkit_odata::ODataQuery {
    let mut q = toolkit_odata::ODataQuery::new();
    if let Some(f) = filter_str {
        let parsed = toolkit_odata::parse_filter_string(f).expect("test $filter must parse");
        q = q.with_filter(parsed.into_expr());
    }
    if let Some(l) = limit {
        q = q.with_limit(l);
    }
    q
}

fn new_input(
    role: Uuid,
    principal_id: &str,
    principal_type: PrincipalType,
    scope: &str,
) -> NewRoleAssignment {
    NewRoleAssignment {
        role_definition_id: role,
        principal_id: principal_id.to_owned(),
        principal_type,
        scope: rbac_sdk::models::Scope::parse(scope).expect("test scope must parse"),
        created_by: "alice".to_owned(),
        // Isolation tests; the author identity plays no part.
        created_by_type: None,
        created_by_tenant_id: None,
    }
}

/// Fresh in-memory SQLite with the migrations applied, plus one custom
/// role definition the assignments can reference (the FK target).
///
/// The Postgres sibling inserts the role with raw SQL against the
/// testcontainer pool; here it goes through the real
/// `RoleDefinitionRepository` over the same provider, which is what the
/// `sqlite_api_*` suites do.
async fn fresh_repo_with_role() -> Result<(
    role_assignment_repo::RoleAssignmentRepository,
    toolkit_db::DBProvider<toolkit_db::DbError>,
    Uuid,
)> {
    let provider = common::fresh_sqlite_provider().await?;
    let role_repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    let role_id = Uuid::now_v7();
    let seed_conn = provider.conn()?;
    role_repo
        .create(
            &seed_conn,
            NewRoleDefinition {
                id: role_id,
                name: format!("AssignmentTestRole-{role_id}"),
                description: Some("isolation fixture".to_owned()),
                permissions: vec![rbac_sdk::models::PermissionRule::new(
                    "read",
                    "gts.cf.resources.compute.vm.v1~",
                )],
                not_permissions: Vec::new(),
                // Root-assignable so every scope used below is legal for it.
                assignable_scopes: vec![rbac_sdk::models::Scope::Root],
                owner_tenant_id: Uuid::now_v7(),
                created_by: "tester".to_owned(),
            },
        )
        .await
        .expect("seeding the FK-target role definition must succeed");

    Ok((
        role_assignment_repo::RoleAssignmentRepository,
        provider,
        role_id,
    ))
}

/// Seed the five-scope fixture both prefix tests share: root, T1, an RG
/// under T1, the sibling-prefix tenant, and an unrelated tenant.
async fn seed_prefix_fixture(
    repo: &role_assignment_repo::RoleAssignmentRepository,
    conn: &toolkit_db::secure::DbConn<'_>,
    role: Uuid,
) {
    for scope in [
        "/".to_owned(),
        format!("/tenants/{T1}"),
        format!("/tenants/{T1}/resourceGroups/{RG1}"),
        format!("/tenants/{T_SIBLING_PREFIX}"),
        format!("/tenants/{T2}"),
    ] {
        repo.create(conn, new_input(role, "p", PrincipalType::User, &scope))
            .await
            .expect("seed");
    }
}

/// The two scopes a T1-rooted prefix query must return: T1 itself and
/// its descendant RG — never the sibling-prefix tenant.
fn expected_t1_subtree() -> Vec<String> {
    let mut v = vec![
        format!("/tenants/{T1}"),
        format!("/tenants/{T1}/resourceGroups/{RG1}"),
    ];
    v.sort();
    v
}

fn sorted_scopes<T: AsRef<[rbac::domain::model::RoleAssignmentModel]>>(items: T) -> Vec<String> {
    let mut scopes: Vec<String> = items.as_ref().iter().map(|r| r.scope.path()).collect();
    scopes.sort();
    scopes
}

#[tokio::test]
async fn scope_prefix_filter_excludes_sibling_prefixes() -> Result<()> {
    let (repo, provider, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    seed_prefix_fixture(&repo, &conn, role).await;

    let filter = format!("scope eq '/tenants/{T1}' or startswith(scope, '/tenants/{T1}/')");
    let page = repo
        .list(
            &conn,
            VisibilityFilter::Unrestricted,
            &build_query(Some(&filter), Some(50)),
        )
        .await
        .expect("list");

    assert_eq!(
        sorted_scopes(page.items),
        expected_t1_subtree(),
        "scope_prefix MUST return descendants only \u{2014} sibling prefix \
         /tenants/{T_SIBLING_PREFIX} MUST NOT match"
    );
    Ok(())
}

#[tokio::test]
async fn visibility_filter_narrows_to_subtrees() -> Result<()> {
    let (repo, provider, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    for scope in [
        format!("/tenants/{T1}"),
        format!("/tenants/{T1}/resourceGroups/{RG1}"),
        format!("/tenants/{T2}"),
    ] {
        repo.create(&conn, new_input(role, "p", PrincipalType::User, &scope))
            .await
            .expect("seed");
    }

    let page = repo
        .list(
            &conn,
            VisibilityFilter::Subtrees(vec![format!("/tenants/{T1}")]),
            &build_query(None, Some(50)),
        )
        .await
        .expect("list");
    assert_eq!(
        sorted_scopes(page.items),
        expected_t1_subtree(),
        "Subtrees visibility MUST NOT admit another tenant's rows"
    );

    // `VisibilityFilter::None` short-circuits — the closed posture.
    let page = repo
        .list(&conn, VisibilityFilter::None, &build_query(None, Some(50)))
        .await
        .expect("list");
    assert!(
        page.items.is_empty(),
        "None visibility MUST yield no rows, got {:?}",
        sorted_scopes(page.items)
    );
    Ok(())
}

#[tokio::test]
async fn visibility_filter_subtrees_excludes_a_sibling_prefix_tenant() -> Result<()> {
    // The `Subtrees` counterpart of the filter-level sibling-prefix case:
    // a visibility prefix built without the trailing separator would
    // admit `/tenants/0000...010` for a caller scoped to
    // `/tenants/0000...01`. Not present in the Postgres file, and it is
    // the visibility path — the one that decides cross-tenant reads.
    let (repo, provider, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let scoped_to = "00000000-0000-0000-0000-000000000001";
    for scope in [
        format!("/tenants/{scoped_to}"),
        format!("/tenants/{T_SIBLING_PREFIX}"),
    ] {
        repo.create(&conn, new_input(role, "p", PrincipalType::User, &scope))
            .await
            .expect("seed");
    }

    let page = repo
        .list(
            &conn,
            VisibilityFilter::Subtrees(vec![format!("/tenants/{scoped_to}")]),
            &build_query(None, Some(50)),
        )
        .await
        .expect("list");

    assert_eq!(
        sorted_scopes(page.items),
        vec![format!("/tenants/{scoped_to}")],
        "a tenant whose id merely shares a textual prefix MUST NOT be visible"
    );
    Ok(())
}

#[tokio::test]
async fn get_subject_assignments_excludes_ancestor_rg_scopes() -> Result<()> {
    let (repo, provider, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let parent = Uuid::now_v7();
    let child = Uuid::now_v7();
    let rg_parent = Uuid::now_v7();
    repo.create(&conn, new_input(role, "alice", PrincipalType::User, "/"))
        .await?;
    repo.create(
        &conn,
        new_input(
            role,
            "alice",
            PrincipalType::User,
            &format!("/tenants/{parent}/resourceGroups/{rg_parent}"),
        ),
    )
    .await?;

    let query = SubjectAssignmentsQuery {
        user_principal: Some((PrincipalType::User, "alice".to_owned())),
        group_principals: Vec::new(),
        ancestor_scopes: vec![
            "/".to_owned(),
            format!("/tenants/{parent}"),
            format!("/tenants/{child}"),
        ],
        context_tenant_rg_prefix: format!("/tenants/{child}/resourceGroups/%"),
        all_scopes: false,
    };
    let rows = repo.get_subject_assignments(&conn, query).await?;
    let scopes = sorted_scopes(rows);

    assert!(
        scopes.contains(&"/".to_owned()),
        "the root grant applies and MUST be returned, got {scopes:?}"
    );
    assert!(
        !scopes.contains(&format!("/tenants/{parent}/resourceGroups/{rg_parent}")),
        "an RG under an ANCESTOR tenant MUST be excluded \u{2014} only the context \
         tenant's own RGs may contribute, got {scopes:?}"
    );
    Ok(())
}
