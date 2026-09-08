//! Integration tests for [`RoleAssignmentRepository`] against a
//! freshly-migrated ephemeral PostgreSQL: SQLSTATE mappings,
//! `scope_prefix` descendant-only semantics, cursor pagination, and
//! visibility filter narrowing.

#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod common;

use anyhow::Result;
use toolkit_db::{ConnectOpts, connect_db};
use uuid::Uuid;

use rbac::domain::error::DomainError;
use rbac::domain::role_assignment_repo::{
    NewRoleAssignment, RoleAssignmentRepository, SubjectAssignmentsQuery, VisibilityFilter,
};
use rbac::infra::storage::role_assignment_repo;
use rbac_sdk::models::PrincipalType;

/// Helper: build an `ODataQuery` for tests. `filter_str` is parsed
/// through `toolkit_odata::parse_filter_string`; `cursor` is the
/// opaque-string form returned by a previous page.
fn build_query(
    filter_str: Option<&str>,
    cursor: Option<String>,
    limit: Option<u64>,
) -> toolkit_odata::ODataQuery {
    let mut q = toolkit_odata::ODataQuery::new();
    if let Some(f) = filter_str {
        let parsed = toolkit_odata::parse_filter_string(f).expect("test $filter must parse");
        q = q.with_filter(parsed.into_expr());
    }
    if let Some(c) = cursor {
        let decoded = toolkit_odata::CursorV1::decode(&c).expect("test cursor must decode");
        q = q.with_cursor(decoded);
    }
    if let Some(l) = limit {
        q = q.with_limit(l);
    }
    q
}

async fn fresh_repo_with_role() -> Result<(
    role_assignment_repo::RoleAssignmentRepository,
    toolkit_db::DBProvider<toolkit_db::DbError>,
    common::PostgresUnderTest,
    Uuid,
)> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let role_id = Uuid::now_v7();
    let tenant = Uuid::now_v7();
    common::insert_canonical_custom_role(&fixture.pool, role_id, "AssignmentTestRole", tenant)
        .await?;
    let db = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider = toolkit_db::DBProvider::<toolkit_db::DbError>::new(db);
    Ok((
        role_assignment_repo::RoleAssignmentRepository,
        provider,
        fixture,
        role_id,
    ))
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
        scope: rbac_sdk::models::Scope::parse(scope).unwrap(),
        created_by: "alice".to_owned(),
        // Visibility-filter tests; the author identity plays no part.
        created_by_type: None,
        created_by_tenant_id: None,
    }
}

/// Build a `RoleAssignmentService` against the given repo. These tests
/// only exercise `list`, so the `RoleDefinitionRepository` and
/// `ScopeValidator` dependencies are stubbed.
fn build_assignment_service(
    provider: &toolkit_db::DBProvider<toolkit_db::DbError>,
    repo: std::sync::Arc<role_assignment_repo::RoleAssignmentRepository>,
    policy: std::sync::Arc<rbac::domain::policy_enforcer::MockPolicyEnforcer>,
) -> std::sync::Arc<
    rbac::domain::role_assignment::RoleAssignmentService<
        role_assignment_repo::RoleAssignmentRepository,
        StubRoleDefinitionRepo,
    >,
> {
    use std::sync::Arc;
    let role_repo = Arc::new(StubRoleDefinitionRepo);
    let tenant_resolver = Arc::new(common::scope_fakes::FakeTenantResolverClient::with_chain(
        &[Uuid::nil()],
    )) as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(NoopRbacRgRead) as Arc<dyn rbac::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(rbac::domain::scope_validator::ScopeValidator::new(
        tenant_resolver,
        rg.clone(),
    ));
    Arc::new(rbac::domain::role_assignment::RoleAssignmentService::new(
        provider.clone(),
        repo,
        role_repo,
        policy,
        scope_validator,
        rg,
    ))
}

/// Stub `RoleDefinitionRepository` for tests that exercise only
/// `RoleAssignmentService::list` — `list` never touches `role_repo`,
/// so every method panics.
struct StubRoleDefinitionRepo;
#[async_trait::async_trait]
impl rbac::domain::role_definition_repo::RoleDefinitionRepository for StubRoleDefinitionRepo {
    async fn create<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _new: rbac::domain::role_definition_repo::NewRoleDefinition,
    ) -> Result<rbac::domain::model::RoleDefinitionModel, rbac::domain::error::DomainError> {
        unreachable!("list path does not call create");
    }
    async fn find_by_id<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<Option<rbac::domain::model::RoleDefinitionModel>, rbac::domain::error::DomainError>
    {
        unreachable!("list path does not call find_by_id");
    }
    async fn find_by_ids<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _ids: &[Uuid],
    ) -> Result<Vec<rbac::domain::model::RoleDefinitionModel>, rbac::domain::error::DomainError>
    {
        unreachable!("list path does not call find_by_ids");
    }
    async fn list<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _visibility: rbac::domain::role_definition_repo::RoleDefinitionVisibility,
        _query: &toolkit_odata::ODataQuery,
    ) -> Result<
        toolkit_odata::Page<rbac::domain::model::RoleDefinitionModel>,
        rbac::domain::error::DomainError,
    > {
        unreachable!("list path does not call repo.list");
    }
    async fn update<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _patch: rbac::domain::role_definition_repo::RoleDefinitionPatch,
        _expected_etag: &rbac::domain::etag::Etag,
    ) -> Result<rbac::domain::model::RoleDefinitionModel, rbac::domain::error::DomainError> {
        unreachable!("list path does not call update");
    }
    async fn delete<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
        _expected_etag: &rbac::domain::etag::Etag,
    ) -> Result<(), rbac::domain::error::DomainError> {
        unreachable!("list path does not call delete");
    }
    async fn count_assignments_for_role<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _id: Uuid,
    ) -> Result<u64, rbac::domain::error::DomainError> {
        unreachable!("list path does not call count_assignments_for_role");
    }
    async fn count_by_type<C: toolkit_db::secure::DBRunner>(
        &self,
        _db: &C,
        _visibility: rbac::domain::role_definition_repo::RoleDefinitionVisibility,
    ) -> Result<rbac::domain::role_definition_repo::RoleTypeCounts, rbac::domain::error::DomainError>
    {
        unreachable!("list path does not summarise the role catalog");
    }
}

/// No-op RbacRgRead — these tests don't exercise RG scopes.
struct NoopRbacRgRead;
#[async_trait::async_trait]
impl rbac::domain::rg_port::RbacRgRead for NoopRbacRgRead {
    async fn get_group(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _id: Uuid,
    ) -> Result<rbac::domain::rg_port::RbacRgGroup, rbac::domain::rg_port::RbacRgReadError> {
        Err(rbac::domain::rg_port::RbacRgReadError::NotFound)
    }

    /// No groups, so no group names. Display-name resolution is not part
    /// of what this test exercises.
    async fn group_names(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, String>, rbac::domain::rg_port::RbacRgReadError>
    {
        Ok(std::collections::HashMap::new())
    }
    async fn list_memberships(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _query: &toolkit_odata::ODataQuery,
    ) -> Result<
        toolkit_odata::Page<rbac::domain::rg_port::RbacRgMembership>,
        rbac::domain::rg_port::RbacRgReadError,
    > {
        Ok(toolkit_odata::Page::new(
            Vec::new(),
            toolkit_odata::PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: 0,
            },
        ))
    }
}

/// I-16 — duplicate `(role, principal_type, principal_id, scope)` tuple
/// maps to `DuplicateAssignment` via `uq_assignment`.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn duplicate_tuple_maps_to_duplicate_assignment() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    repo.create(
        &conn,
        new_input(
            role,
            "alice",
            PrincipalType::User,
            "/tenants/11111111-1111-1111-1111-111111111111",
        ),
    )
    .await
    .expect("first create");
    let err = repo
        .create(
            &conn,
            new_input(
                role,
                "alice",
                PrincipalType::User,
                "/tenants/11111111-1111-1111-1111-111111111111",
            ),
        )
        .await
        .expect_err("duplicate MUST fail");
    assert!(
        matches!(
            err,
            DomainError::RoleAssignmentDuplicate {
                role_definition_id,
                ref principal_type,
                ref principal_id,
                ref scope,
            } if role_definition_id == role
                && *principal_type == PrincipalType::User
                && principal_id == "alice"
                && scope == "/tenants/11111111-1111-1111-1111-111111111111"
        ),
        "expected DuplicateAssignment, got {err:?}"
    );
    Ok(())
}

/// I-X — insert with a dangling `role_definition_id` maps to
/// `RoleDefinitionMissing` via the `role_assignments_role_definition_id_fkey`
/// FK violation.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn dangling_role_definition_id_maps_to_role_definition_missing() -> Result<()> {
    let (repo, provider, _fixture, _role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let bogus_role = Uuid::now_v7();
    let err = repo
        .create(
            &conn,
            new_input(
                bogus_role,
                "alice",
                PrincipalType::User,
                "/tenants/11111111-1111-1111-1111-111111111111",
            ),
        )
        .await
        .expect_err("FK violation MUST fail");
    assert!(
        matches!(
            err,
            DomainError::RoleDefinitionMissing { role_definition_id }
            if role_definition_id == bogus_role
        ),
        "expected RoleDefinitionMissing, got {err:?}"
    );
    Ok(())
}

/// I-15 — create + find_by_id round-trip persists every field.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn create_then_find_by_id_round_trips() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let created = repo
        .create(
            &conn,
            new_input(
                role,
                "alice",
                PrincipalType::User,
                "/tenants/11111111-1111-1111-1111-111111111111",
            ),
        )
        .await
        .expect("create");

    let fetched = repo
        .find_by_id(&conn, created.id)
        .await
        .expect("find_by_id")
        .expect("present");
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.role_definition_id, role);
    assert_eq!(fetched.principal_id, "alice");
    assert_eq!(fetched.principal_type, PrincipalType::User);
    assert_eq!(
        fetched.scope.path(),
        "/tenants/11111111-1111-1111-1111-111111111111"
    );
    // updated_at == created_at for the row's lifetime (no PATCH).
    assert_eq!(fetched.updated_at, fetched.created_at);
    Ok(())
}

/// I-22 — `scope_prefix` returns descendants only; sibling-prefix
/// strings do not match (trailing-slash sentinel kicks in SQL-side).
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn scope_prefix_filter_excludes_sibling_prefixes() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    for scope in [
        "/",
        "/tenants/11111111-1111-1111-1111-111111111111",
        "/tenants/11111111-1111-1111-1111-111111111111/resourceGroups/11111111-aaaa-bbbb-cccc-111111111111",
        "/tenants/00000000-0000-0000-0000-000000000010", // sibling-prefix candidate
        "/tenants/22222222-2222-2222-2222-222222222222",
    ] {
        repo.create(&conn, new_input(role, "p", PrincipalType::User, scope))
            .await
            .expect("seed");
    }

    let page = repo
        .list(
            &conn,
            VisibilityFilter::Unrestricted,
            &build_query(
                Some(
                    "scope eq '/tenants/11111111-1111-1111-1111-111111111111' or \
                     startswith(scope, '/tenants/11111111-1111-1111-1111-111111111111/')",
                ),
                None,
                Some(50),
            ),
        )
        .await
        .expect("list");

    let mut scopes: Vec<String> = page.items.iter().map(|r| r.scope.path()).collect();
    scopes.sort();
    assert_eq!(
        scopes,
        vec![
            "/tenants/11111111-1111-1111-1111-111111111111".to_owned(),
            "/tenants/11111111-1111-1111-1111-111111111111/resourceGroups/11111111-aaaa-bbbb-cccc-111111111111".to_owned(),
        ],
        "scope_prefix MUST return descendants only \u{2014} sibling prefix /tenants/00000000-0000-0000-0000-000000000010 MUST NOT match"
    );
    Ok(())
}

/// I-23 — cursor pagination over 100+ rows: every row appears exactly once.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn cursor_pagination_over_100_rows_no_duplicates_no_gaps() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    // Repo clamps `created_at = now()`; many rows can share a microsecond,
    // so assert duplicate-freedom via set equality on `id`.
    let mut seeded_ids: Vec<Uuid> = Vec::with_capacity(120);
    for i in 0..120 {
        let row = repo
            .create(
                &conn,
                new_input(
                    role,
                    &format!("p{i:03}"),
                    PrincipalType::User,
                    "/tenants/11111111-1111-1111-1111-111111111111",
                ),
            )
            .await
            .expect("seed");
        seeded_ids.push(row.id);
    }

    let page_size: u64 = 25;
    let mut seen: Vec<Uuid> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = repo
            .list(
                &conn,
                VisibilityFilter::Unrestricted,
                &build_query(None, cursor.clone(), Some(page_size)),
            )
            .await
            .expect("list page");
        seen.extend(page.items.iter().map(|r| r.id));
        match page.page_info.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    seeded_ids.sort();
    let mut sorted_seen = seen.clone();
    sorted_seen.sort();
    sorted_seen.dedup();
    assert_eq!(
        sorted_seen.len(),
        seen.len(),
        "page sequence MUST have no duplicates: {} unique vs {} seen",
        sorted_seen.len(),
        seen.len()
    );
    assert_eq!(
        sorted_seen, seeded_ids,
        "page sequence MUST visit every seeded row exactly once"
    );
    Ok(())
}

/// I-26 — delete removes the row; second delete returns `false`.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn delete_removes_row() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let created = repo
        .create(
            &conn,
            new_input(
                role,
                "alice",
                PrincipalType::User,
                "/tenants/11111111-1111-1111-1111-111111111111",
            ),
        )
        .await
        .expect("create");
    assert!(repo.delete(&conn, created.id).await.expect("first delete"));
    assert!(!repo.delete(&conn, created.id).await.expect("second delete"));
    assert!(
        repo.find_by_id(&conn, created.id)
            .await
            .expect("find_by_id")
            .is_none()
    );
    Ok(())
}

/// I-25 — visibility filter narrows rows to the configured subtrees.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn visibility_filter_narrows_to_subtrees() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    repo.create(
        &conn,
        new_input(
            role,
            "p",
            PrincipalType::User,
            "/tenants/11111111-1111-1111-1111-111111111111",
        ),
    )
    .await
    .expect("t1");
    repo.create(&conn, new_input(
        role,
        "p",
        PrincipalType::User,
        "/tenants/11111111-1111-1111-1111-111111111111/resourceGroups/11111111-aaaa-bbbb-cccc-111111111111",
    ))
    .await
    .expect("rg1");
    repo.create(
        &conn,
        new_input(
            role,
            "p",
            PrincipalType::User,
            "/tenants/22222222-2222-2222-2222-222222222222",
        ),
    )
    .await
    .expect("t2");

    let page = repo
        .list(
            &conn,
            VisibilityFilter::Subtrees(vec![
                "/tenants/11111111-1111-1111-1111-111111111111".to_owned(),
            ]),
            &build_query(None, None, Some(50)),
        )
        .await
        .expect("list");
    let mut scopes: Vec<String> = page.items.iter().map(|r| r.scope.path()).collect();
    scopes.sort();
    assert_eq!(
        scopes,
        vec![
            "/tenants/11111111-1111-1111-1111-111111111111".to_owned(),
            "/tenants/11111111-1111-1111-1111-111111111111/resourceGroups/11111111-aaaa-bbbb-cccc-111111111111".to_owned(),
        ]
    );

    // VisibilityFilter::None short-circuits.
    let page = repo
        .list(
            &conn,
            VisibilityFilter::None,
            &build_query(None, None, Some(50)),
        )
        .await
        .expect("list");
    assert!(page.items.is_empty());
    Ok(())
}

/// I-33 — two-phase scope predicate returns ancestor IN matches plus
/// the context-tenant RG LIKE matches in one statement.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_assignments_two_phase_ancestors_plus_rg() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let parent = Uuid::now_v7();
    let child = Uuid::now_v7();
    let rg1 = Uuid::now_v7();
    repo.create(&conn, new_input(role, "alice", PrincipalType::User, "/"))
        .await?;
    repo.create(
        &conn,
        new_input(
            role,
            "alice",
            PrincipalType::User,
            &format!("/tenants/{parent}"),
        ),
    )
    .await?;
    repo.create(
        &conn,
        new_input(
            role,
            "alice",
            PrincipalType::User,
            &format!("/tenants/{child}"),
        ),
    )
    .await?;
    repo.create(
        &conn,
        new_input(
            role,
            "alice",
            PrincipalType::User,
            &format!("/tenants/{child}/resourceGroups/{rg1}"),
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
    let scopes: Vec<String> = rows.iter().map(|r| r.scope.path()).collect();
    assert_eq!(scopes.len(), 4, "expected 4 rows, got {scopes:?}");
    assert!(scopes.contains(&"/".to_owned()));
    assert!(scopes.contains(&format!("/tenants/{parent}")));
    assert!(scopes.contains(&format!("/tenants/{child}")));
    assert!(scopes.contains(&format!("/tenants/{child}/resourceGroups/{rg1}")));
    Ok(())
}

/// `all_scopes: true` aggregates a subject's grants across EVERY tenant, not
/// just the ancestor chain in `ancestor_scopes`. A grant in an *unrelated*
/// tenant (no ancestor relationship) is returned with `all_scopes: true` and
/// omitted with `all_scopes: false`. This is the root-context list path:
/// collapsing `Scope::Root` to the home tenant would silently drop
/// cross-tenant grants.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn all_scopes_query_aggregates_grants_across_tenants() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7(); // unrelated to A — neither is the other's ancestor
    repo.create(
        &conn,
        new_input(
            role,
            "alice",
            PrincipalType::User,
            &format!("/tenants/{tenant_a}"),
        ),
    )
    .await?;
    repo.create(
        &conn,
        new_input(
            role,
            "alice",
            PrincipalType::User,
            &format!("/tenants/{tenant_b}"),
        ),
    )
    .await?;

    // all_scopes: true → both tenants' grants surface regardless of the
    // (empty) ancestor-scope set.
    let all = repo
        .get_subject_assignments(
            &conn,
            SubjectAssignmentsQuery {
                user_principal: Some((PrincipalType::User, "alice".to_owned())),
                group_principals: Vec::new(),
                ancestor_scopes: Vec::new(),
                context_tenant_rg_prefix: String::new(),
                all_scopes: true,
            },
        )
        .await?;
    let all_scopes: Vec<String> = all.iter().map(|r| r.scope.path()).collect();
    assert!(
        all_scopes.contains(&format!("/tenants/{tenant_a}"))
            && all_scopes.contains(&format!("/tenants/{tenant_b}")),
        "all_scopes=true MUST surface grants in both tenants, got {all_scopes:?}"
    );

    // all_scopes: false narrowed to tenant A's chain → only A; B omitted.
    let scoped = repo
        .get_subject_assignments(
            &conn,
            SubjectAssignmentsQuery {
                user_principal: Some((PrincipalType::User, "alice".to_owned())),
                group_principals: Vec::new(),
                ancestor_scopes: vec![format!("/tenants/{tenant_a}")],
                context_tenant_rg_prefix: format!("/tenants/{tenant_a}/resourceGroups/%"),
                all_scopes: false,
            },
        )
        .await?;
    let scoped_scopes: Vec<String> = scoped.iter().map(|r| r.scope.path()).collect();
    assert!(
        scoped_scopes.contains(&format!("/tenants/{tenant_a}")),
        "all_scopes=false MUST still return the in-chain grant (A), got {scoped_scopes:?}"
    );
    assert!(
        !scoped_scopes.contains(&format!("/tenants/{tenant_b}")),
        "all_scopes=false MUST narrow to the ancestor chain and omit the unrelated tenant (B), \
         got {scoped_scopes:?}"
    );
    Ok(())
}

/// A subject in MORE than `GROUP_PRINCIPALS_CHUNK` (500) groups must
/// still get every matching assignment back. The chunked `IN (...)` union
/// returns the full set (the direct user row + all group rows), dropping
/// no rows at the chunk boundary and producing no duplicates.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_assignments_chunks_large_group_set_without_dropping_rows() -> Result<()> {
    // MUST exceed GROUP_PRINCIPALS_CHUNK (500) so the query splits into ≥2
    // chunks and the union path is exercised.
    const N_GROUPS: usize = 501;

    let (repo, provider, fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let tenant = Uuid::now_v7();
    let scope = format!("/tenants/{tenant}");

    // One direct user assignment.
    repo.create(&conn, new_input(role, "alice", PrincipalType::User, &scope))
        .await?;

    // 501 group assignments at the same tenant scope, bulk-inserted in one
    // statement for speed.
    let group_ids: Vec<String> = (0..N_GROUPS).map(|_| Uuid::now_v7().to_string()).collect();
    sqlx::query(
        "INSERT INTO role_assignments \
         (id, role_definition_id, principal_id, principal_type, scope, scope_depth, tenant_id, created_by) \
         SELECT gen_random_uuid(), $1, pid, 'Group', $2, 2, $3, 'tester' \
         FROM unnest($4::text[]) AS pid",
    )
    .bind(role)
    .bind(&scope)
    .bind(tenant)
    .bind(&group_ids)
    .execute(&fixture.pool)
    .await?;

    let query = SubjectAssignmentsQuery {
        user_principal: Some((PrincipalType::User, "alice".to_owned())),
        group_principals: group_ids.clone(),
        ancestor_scopes: vec!["/".to_owned(), scope.clone()],
        context_tenant_rg_prefix: format!("/tenants/{tenant}/resourceGroups/%"),
        all_scopes: false,
    };
    let rows = repo.get_subject_assignments(&conn, query).await?;

    assert_eq!(
        rows.len(),
        N_GROUPS + 1,
        "expected the user row + all {N_GROUPS} group rows across chunks, got {}",
        rows.len()
    );
    let unique_ids: std::collections::HashSet<Uuid> = rows.iter().map(|r| r.id).collect();
    assert_eq!(
        unique_ids.len(),
        rows.len(),
        "the chunked union MUST NOT produce duplicate rows"
    );
    assert!(
        rows.iter().any(|r| r.principal_id == "alice"),
        "the direct user assignment MUST be present (it is queried once, not per chunk)"
    );
    let group_rows = rows
        .iter()
        .filter(|r| r.principal_type == PrincipalType::Group)
        .count();
    assert_eq!(
        group_rows, N_GROUPS,
        "every group assignment MUST be present across the chunk boundary"
    );
    Ok(())
}

/// I-33b — RG-scoped assignment under an ancestor tenant is excluded.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_assignments_excludes_ancestor_rg_scopes() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
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
    let scopes: Vec<String> = rows.iter().map(|r| r.scope.path()).collect();
    assert!(scopes.contains(&"/".to_owned()));
    assert!(
        !scopes.contains(&format!("/tenants/{parent}/resourceGroups/{rg_parent}")),
        "ancestor-tenant RG must be excluded, got {scopes:?}"
    );
    Ok(())
}

/// I-34 — Group-principal expansion: a Group assignment for one of the
/// supplied group ids is returned with `principal_type = Group`.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_assignments_includes_group_principals() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let group_id = Uuid::now_v7();
    let child = Uuid::now_v7();
    repo.create(
        &conn,
        new_input(
            role,
            &group_id.to_string(),
            PrincipalType::Group,
            &format!("/tenants/{child}"),
        ),
    )
    .await?;

    let query = SubjectAssignmentsQuery {
        user_principal: Some((PrincipalType::User, "alice".to_owned())),
        group_principals: vec![group_id.to_string()],
        ancestor_scopes: vec!["/".to_owned(), format!("/tenants/{child}")],
        context_tenant_rg_prefix: format!("/tenants/{child}/resourceGroups/%"),
        all_scopes: false,
    };
    let rows = repo.get_subject_assignments(&conn, query).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].principal_id, group_id.to_string());
    assert_eq!(rows[0].principal_type, PrincipalType::Group);
    Ok(())
}

// =============================================================================
// Handler-level tests — need real state across two calls.
// =============================================================================

/// Handler-level descendant-prefix semantics — exercises the
/// `ListRoleAssignments` handler so the visibility-filter / readable-scopes
/// translation is also covered.
#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn scope_prefix_returns_descendants_only_and_excludes_siblings() -> Result<()> {
    use rbac::domain::policy_enforcer::{MockPolicyEnforcer, ReadableScopes, ReadableScopesPred};
    use rbac::domain::role_assignment::ListRoleAssignmentsRequest;
    use std::sync::Arc;

    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let repo_dyn = Arc::new(repo);
    for scope in [
        "/",
        "/tenants/11111111-1111-1111-1111-111111111111",
        "/tenants/11111111-1111-1111-1111-111111111111/resourceGroups/11111111-aaaa-bbbb-cccc-111111111111",
        "/tenants/00000000-0000-0000-0000-000000000010", // sibling-prefix candidate
        "/tenants/22222222-2222-2222-2222-222222222222",
    ] {
        repo_dyn
            .create(&conn, new_input(role, "p", PrincipalType::User, scope))
            .await
            .expect("seed");
    }

    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]));
    let service = build_assignment_service(&provider, Arc::clone(&repo_dyn), policy);
    let page = service
        .list(
            &toolkit_security::SecurityContext::anonymous(),
            ListRoleAssignmentsRequest {
                context_scope: rbac_sdk::models::Scope::Root,
                query: build_query(
                    Some(
                        "scope eq '/tenants/11111111-1111-1111-1111-111111111111' or \
                         startswith(scope, '/tenants/11111111-1111-1111-1111-111111111111/')",
                    ),
                    None,
                    Some(50),
                ),
            },
        )
        .await
        .expect("list");
    let mut scopes: Vec<String> = page.items.iter().map(|r| r.scope.path()).collect();
    scopes.sort();
    assert_eq!(
        scopes,
        vec![
            "/tenants/11111111-1111-1111-1111-111111111111".to_owned(),
            "/tenants/11111111-1111-1111-1111-111111111111/resourceGroups/11111111-aaaa-bbbb-cccc-111111111111".to_owned(),
        ],
        "scope_prefix MUST return descendants only \u{2014} sibling prefix /tenants/00000000-0000-0000-0000-000000000010 MUST NOT match"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn cursor_round_trip_across_two_pages() -> Result<()> {
    use rbac::domain::policy_enforcer::{MockPolicyEnforcer, ReadableScopes, ReadableScopesPred};
    use rbac::domain::role_assignment::ListRoleAssignmentsRequest;
    use std::sync::Arc;

    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let repo_dyn = Arc::new(repo);
    for i in 0..6_u32 {
        repo_dyn
            .create(
                &conn,
                new_input(
                    role,
                    &format!("p{i:03}"),
                    PrincipalType::User,
                    "/tenants/11111111-1111-1111-1111-111111111111",
                ),
            )
            .await
            .expect("seed");
    }

    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]));
    let service = build_assignment_service(&provider, Arc::clone(&repo_dyn), policy);
    let page1 = service
        .list(
            &toolkit_security::SecurityContext::anonymous(),
            ListRoleAssignmentsRequest {
                context_scope: rbac_sdk::models::Scope::Root,
                query: build_query(None, None, Some(3)),
            },
        )
        .await
        .expect("page 1");
    let cursor = page1
        .page_info
        .next_cursor
        .clone()
        .expect("page 1 MUST emit cursor");
    let page1_ids: std::collections::HashSet<Uuid> = page1.items.iter().map(|r| r.id).collect();
    let page2 = service
        .list(
            &toolkit_security::SecurityContext::anonymous(),
            ListRoleAssignmentsRequest {
                context_scope: rbac_sdk::models::Scope::Root,
                query: build_query(None, Some(cursor), Some(3)),
            },
        )
        .await
        .expect("page 2");
    for r in &page2.items {
        assert!(
            !page1_ids.contains(&r.id),
            "id {} appears on both pages; cursor must walk forward",
            r.id
        );
    }
    Ok(())
}

/// I-35 — ordering by `(scope_depth DESC, id DESC)` is index-backed
/// (composite `idx_role_assignments_principal_scope_depth` or fallback
/// `idx_role_assignments_scope_depth`) with no explicit Sort step and no
/// per-row `char_length(scope)`.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_assignments_ordering_is_index_backed() -> Result<()> {
    let (repo, provider, fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let child = Uuid::now_v7();
    repo.create(&conn, new_input(role, "alice", PrincipalType::User, "/"))
        .await?;
    repo.create(
        &conn,
        new_input(
            role,
            "alice",
            PrincipalType::User,
            &format!("/tenants/{child}"),
        ),
    )
    .await?;

    // Assert on EXPLAIN text so a SQL refactor fails loud.
    let explain_sql = format!(
        "EXPLAIN (ANALYZE, BUFFERS) \
         SELECT id, role_definition_id, principal_id, principal_type, scope, \
                created_at, updated_at, created_by \
         FROM role_assignments \
         WHERE ((principal_type = 'User' AND principal_id = 'alice') \
              OR (principal_type = 'Group' AND principal_id = ANY(ARRAY[]::text[]))) \
           AND (scope IN ('/', '/tenants/{child}') \
                OR scope LIKE '/tenants/{child}/resourceGroups/%') \
         ORDER BY scope_depth DESC, id DESC"
    );
    // Strip the empty `ARRAY[]::text[]` branch (production omits it).
    let explain_sql_no_groups = explain_sql.replace(
        "OR (principal_type = 'Group' AND principal_id = ANY(ARRAY[]::text[]))",
        "",
    );
    // sqlx 0.9 accepts only `&'static str`; asserted safe because the only interpolated
    // value is the locally generated `child` UUID — no external input reaches the string.
    let rows: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(explain_sql_no_groups))
        .fetch_all(&fixture.pool)
        .await?;
    let plan: String = rows
        .into_iter()
        .map(|(line,)| line)
        .collect::<Vec<_>>()
        .join("\n");
    let uses_composite = plan.contains("idx_role_assignments_principal_scope_depth");
    let uses_scope_depth_only = plan.contains("idx_role_assignments_scope_depth")
        && !plan.contains("idx_role_assignments_principal_scope_depth");
    assert!(
        uses_composite || uses_scope_depth_only,
        "expected plan to use idx_role_assignments_principal_scope_depth or \
         idx_role_assignments_scope_depth, got:\n{plan}"
    );
    assert!(
        !plan.contains("Sort Key:"),
        "expected ordering to be index-backed (no explicit Sort step), got:\n{plan}"
    );
    assert!(
        !plan.contains("char_length(scope)"),
        "expected ordering to avoid per-row char_length(scope), got:\n{plan}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `count_by_role` — grouped, visibility-bounded counting
// ---------------------------------------------------------------------------

const T1: &str = "11111111-1111-1111-1111-111111111111";
const T2: &str = "22222222-2222-2222-2222-222222222222";

/// Seed a second custom role alongside the fixture's, so the `GROUP BY` has
/// more than one bucket to get wrong.
async fn seed_second_role(fixture: &common::PostgresUnderTest) -> Result<Uuid> {
    let id = Uuid::now_v7();
    common::insert_canonical_custom_role(
        &fixture.pool,
        id,
        "SecondAssignmentTestRole",
        Uuid::now_v7(),
    )
    .await?;
    Ok(id)
}

/// The grouped count returns one entry per role with the hand-counted number
/// of rows, narrowed to the caller's scope prefixes — and a role with no
/// matching rows is **absent** from the map (the service turns that into
/// `Some(0)`; the repo does not invent zeros).
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p rbac -- --ignored`"]
async fn count_by_role_groups_per_role_under_a_prefix_set() -> Result<()> {
    let (repo, provider, fixture, role_a) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let role_b = seed_second_role(&fixture).await?;
    let role_c = seed_second_role(&fixture).await?;

    // role_a: two rows in T1 plus one under a T1 resource group (a
    // descendant, so the prefix must admit it) = 3.
    for principal in ["a1", "a2"] {
        repo.create(
            &conn,
            new_input(
                role_a,
                principal,
                PrincipalType::User,
                &format!("/tenants/{T1}"),
            ),
        )
        .await
        .expect("seed role_a tenant row");
    }
    repo.create(
        &conn,
        new_input(
            role_a,
            "a3",
            PrincipalType::User,
            &format!("/tenants/{T1}/resourceGroups/11111111-aaaa-bbbb-cccc-111111111111"),
        ),
    )
    .await
    .expect("seed role_a rg row");
    // role_b: one row in T1.
    repo.create(
        &conn,
        new_input(role_b, "b1", PrincipalType::User, &format!("/tenants/{T1}")),
    )
    .await
    .expect("seed role_b");
    // role_c: nothing — it must not appear in the map at all.

    let counts = repo
        .count_by_role(
            &conn,
            VisibilityFilter::Subtrees(vec![format!("/tenants/{T1}")]),
            &[role_a, role_b, role_c],
        )
        .await
        .expect("count");

    assert_eq!(
        counts.get(&role_a).copied(),
        Some(3),
        "two tenant rows plus one descendant RG row; counts={counts:?}"
    );
    assert_eq!(counts.get(&role_b).copied(), Some(1), "counts={counts:?}");
    assert!(
        !counts.contains_key(&role_c),
        "a role with no visible assignments MUST be absent, not zero; counts={counts:?}"
    );
    Ok(())
}

/// A prefix set that names only T1 MUST NOT count T2's rows. This is the
/// property the whole feature rests on: the number a tenant admin reads must
/// not describe another tenant's activity.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p rbac -- --ignored`"]
async fn count_by_role_excludes_rows_outside_the_prefix_set() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    repo.create(
        &conn,
        new_input(
            role,
            "in-t1",
            PrincipalType::User,
            &format!("/tenants/{T1}"),
        ),
    )
    .await
    .expect("seed T1");
    for principal in ["out-1", "out-2"] {
        repo.create(
            &conn,
            new_input(
                role,
                principal,
                PrincipalType::User,
                &format!("/tenants/{T2}"),
            ),
        )
        .await
        .expect("seed T2");
    }

    let counts = repo
        .count_by_role(
            &conn,
            VisibilityFilter::Subtrees(vec![format!("/tenants/{T1}")]),
            &[role],
        )
        .await
        .expect("count");
    assert_eq!(
        counts.get(&role).copied(),
        Some(1),
        "only the T1 row is visible under a T1-only prefix set; counts={counts:?}"
    );

    // The same rows, counted with unrestricted visibility, total three — so
    // the narrowing above is the predicate doing its job, not missing rows.
    let all = repo
        .count_by_role(&conn, VisibilityFilter::Unrestricted, &[role])
        .await
        .expect("count unrestricted");
    assert_eq!(
        all.get(&role).copied(),
        Some(3),
        "Unrestricted counts every row regardless of tenant; counts={all:?}"
    );

    // `None` visibility admits no rows at all, so no role has a number.
    let none = repo
        .count_by_role(&conn, VisibilityFilter::None, &[role])
        .await
        .expect("count none");
    assert!(
        none.is_empty(),
        "VisibilityFilter::None MUST yield an empty map; counts={none:?}"
    );
    Ok(())
}

/// An empty id list short-circuits before SQL. The assertion is not
/// cosmetic: without the guard the query would emit `role_definition_id IN ()`,
/// which is a syntax error on Postgres, so a regression here fails loudly
/// rather than returning a wrong number.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p rbac -- --ignored`"]
async fn count_by_role_with_no_ids_issues_no_query() -> Result<()> {
    let (repo, provider, _fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    // Rows exist, so an unguarded implementation could not answer "empty" by
    // accident.
    repo.create(
        &conn,
        new_input(
            role,
            "someone",
            PrincipalType::User,
            &format!("/tenants/{T1}"),
        ),
    )
    .await
    .expect("seed");

    let counts = repo
        .count_by_role(&conn, VisibilityFilter::Unrestricted, &[])
        .await
        .expect("an empty id list MUST NOT reach the database");
    assert!(counts.is_empty(), "counts={counts:?}");
    Ok(())
}
