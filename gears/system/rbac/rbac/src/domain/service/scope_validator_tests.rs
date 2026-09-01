//! Unit tests for `domain/service/scope_validator.rs`. Fakes
//! (`FakeTenantResolverClient`, `FakeRbacRgRead`) are defined in
//! `scope_fakes.rs` and mirrored under `tests/common/` for the
//! integration suite.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tenant_resolver_sdk::error::TenantResolverError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{
    MissingScopeEntity, Scope, ScopeError, ScopeValidator, UpstreamScopeError, parse_scope,
};
use crate::domain::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};

/// Build an anonymous `SecurityContext` for tests that need one.
fn ctx() -> SecurityContext {
    SecurityContext::anonymous()
}

// ---------------------------------------------------------------------------
// Group 2: parse_scope parser (tasks 2.2–2.11)
// ---------------------------------------------------------------------------

/// 2.2 — Root form: the literal "/" must resolve to `Scope::Root`.
#[test]
fn parse_scope_root() {
    assert_eq!(parse_scope("/"), Ok(Scope::Root));
}

/// 2.3 — Tenant form: a canonical hyphenated UUID after "/tenants/" produces
/// `Scope::Tenant` carrying the exact parsed UUID.
#[test]
fn parse_scope_tenant() {
    let id = Uuid::new_v4();
    let path = format!("/tenants/{id}");
    assert_eq!(parse_scope(&path), Ok(Scope::Tenant { tenant_id: id }));
}

/// 2.4 — RG form: two canonical UUIDs in the full RG path produce
/// `Scope::ResourceGroup` with both UUIDs preserved.
#[test]
fn parse_scope_resource_group() {
    let t_id = Uuid::new_v4();
    let g_id = Uuid::new_v4();
    let path = format!("/tenants/{t_id}/resourceGroups/{g_id}");
    assert_eq!(
        parse_scope(&path),
        Ok(Scope::ResourceGroup {
            tenant_id: t_id,
            group_id: g_id,
        })
    );
}

/// 2.5 — U-52: unknown top-level path returns `InvalidScopeFormat` carrying
/// the original string verbatim.
#[test]
fn parse_scope_unknown_top_level_path() {
    let input = "/foo/bar";
    assert_eq!(
        parse_scope(input),
        Err(ScopeError::InvalidScopeFormat {
            scope: input.to_owned()
        })
    );
}

/// Malformed tenant UUID: the path looks structurally correct but the
/// UUID segment is not a valid hyphenated UUID.
#[test]
fn parse_scope_malformed_tenant_uuid() {
    let input = "/tenants/not-a-uuid";
    assert_eq!(
        parse_scope(input),
        Err(ScopeError::InvalidScopeFormat {
            scope: input.to_owned()
        })
    );
}

/// 2.7 — Malformed RG UUID: tenant UUID is valid but the resource-group UUID
/// is not.
#[test]
fn parse_scope_malformed_rg_uuid() {
    let t_id = Uuid::new_v4();
    let input = format!("/tenants/{t_id}/resourceGroups/not-a-uuid");
    assert!(matches!(
        parse_scope(&input),
        Err(ScopeError::InvalidScopeFormat { .. })
    ));
}

/// 2.8 — Trailing path after the RG UUID is rejected even when all three
/// UUID/literal segments are valid.
#[test]
fn parse_scope_trailing_path_after_rg() {
    let t_id = Uuid::new_v4();
    let g_id = Uuid::new_v4();
    let input = format!("/tenants/{t_id}/resourceGroups/{g_id}/extra");
    assert!(matches!(
        parse_scope(&input),
        Err(ScopeError::InvalidScopeFormat { .. })
    ));
}

/// 2.9 — Wrong middle literal ("groups" instead of "resourceGroups") must be
/// rejected; guards the exact literal requirement.
#[test]
fn parse_scope_wrong_middle_literal() {
    let t_id = Uuid::new_v4();
    let g_id = Uuid::new_v4();
    let input = format!("/tenants/{t_id}/groups/{g_id}");
    assert!(matches!(
        parse_scope(&input),
        Err(ScopeError::InvalidScopeFormat { .. })
    ));
}

/// 2.10 — Empty string and double-slash both reject with `InvalidScopeFormat`.
#[test]
fn parse_scope_empty_and_double_slash() {
    assert!(matches!(
        parse_scope(""),
        Err(ScopeError::InvalidScopeFormat { .. })
    ));
    assert!(matches!(
        parse_scope("//"),
        Err(ScopeError::InvalidScopeFormat { .. })
    ));
}

/// 2.11 — Trailing slash after a valid tenant UUID ("/tenants/{uuid}/") is
/// rejected; guards against the empty-next-segment mis-parse where the path
/// looks like it could be a two-segment tenant + blank RG form.
#[test]
fn parse_scope_trailing_slash_on_tenant() {
    let t_id = Uuid::new_v4();
    let input = format!("/tenants/{t_id}/");
    assert!(matches!(
        parse_scope(&input),
        Err(ScopeError::InvalidScopeFormat { .. })
    ));
}

// ---------------------------------------------------------------------------
// Group 4: validate_scope_exists (tasks 4.5–4.13)
// ---------------------------------------------------------------------------

/// 4.5 — U-47: root scope `/` always exists and makes no external calls.
#[tokio::test]
async fn validate_scope_exists_root() {
    let tenant_fake = Arc::new(FakeTenantResolverClient::with_chain(&[]));
    let rg_fake = Arc::new(FakeRbacRgRead::default());
    let validator = ScopeValidator::new(
        tenant_fake.clone() as Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        rg_fake.clone() as Arc<dyn crate::domain::rg_port::RbacRgRead>,
    );
    assert_eq!(validator.validate_scope_exists(&ctx(), "/").await, Ok(()));
    assert_eq!(
        tenant_fake.total_calls(),
        0,
        "root must not call tenant resolver"
    );
    assert_eq!(rg_fake.get_group_calls.load(Ordering::SeqCst), 0);
}

/// 4.6 — U-48: known tenant scope returns `Ok(())`.
#[tokio::test]
async fn validate_scope_exists_tenant_exists() {
    let t1 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let scope = format!("/tenants/{t1}");
    assert_eq!(
        validator.validate_scope_exists(&ctx(), &scope).await,
        Ok(())
    );
}

/// 4.7 — U-49: unknown tenant returns `ScopeNotFound` with
/// `MissingScopeEntity::Tenant { id }` carrying the exact missing UUID.
#[tokio::test]
async fn validate_scope_exists_tenant_not_found() {
    let t_missing = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let scope = format!("/tenants/{t_missing}");
    assert!(matches!(
        validator.validate_scope_exists(&ctx(), &scope).await,
        Err(ScopeError::ScopeNotFound {
            missing: MissingScopeEntity::Tenant { id },
            ..
        }) if id == t_missing
    ));
}

/// 4.8 — U-50: RG happy path — tenant and group both exist, ownership matches.
#[tokio::test]
async fn validate_scope_exists_rg_happy_path() {
    let t1 = Uuid::new_v4();
    let rg1 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])),
        Arc::new(FakeRbacRgRead::default().with_group(rg1, t1)),
    );
    let scope = format!("/tenants/{t1}/resourceGroups/{rg1}");
    assert_eq!(
        validator.validate_scope_exists(&ctx(), &scope).await,
        Ok(())
    );
}

/// 4.9 — U-51a: tenant exists but the RG UUID is unknown.
#[tokio::test]
async fn validate_scope_exists_rg_not_found() {
    let t1 = Uuid::new_v4();
    let rg_missing = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])),
        Arc::new(FakeRbacRgRead::default()), // empty — no groups seeded
    );
    let scope = format!("/tenants/{t1}/resourceGroups/{rg_missing}");
    assert!(matches!(
        validator.validate_scope_exists(&ctx(), &scope).await,
        Err(ScopeError::ScopeNotFound {
            missing: MissingScopeEntity::ResourceGroup { id },
            ..
        }) if id == rg_missing
    ));
}

/// 4.10 — U-51b: RG exists but is owned by a different tenant (T2 ≠ T1).
#[tokio::test]
async fn validate_scope_exists_rg_ownership_mismatch() {
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let rg1 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])),
        Arc::new(FakeRbacRgRead::default().with_group(rg1, t2)), // rg1 owned by T2, not T1
    );
    let scope = format!("/tenants/{t1}/resourceGroups/{rg1}");
    assert!(matches!(
        validator.validate_scope_exists(&ctx(), &scope).await,
        Err(ScopeError::ScopeNotFound {
            missing: MissingScopeEntity::ResourceGroupOwnerMismatch {
                rg_id,
                claimed_tenant_id,
                actual_tenant_id,
            },
            ..
        }) if rg_id == rg1 && claimed_tenant_id == t1 && actual_tenant_id == t2
    ));
}

/// A missing tenant for an RG scope must short-circuit — the
/// `get_group` counter MUST remain at zero (tenant check runs first).
#[tokio::test]
async fn validate_scope_exists_rg_missing_tenant_short_circuits() {
    let t_missing = Uuid::new_v4();
    let rg1 = Uuid::new_v4();
    let rg_fake = Arc::new(FakeRbacRgRead::default());
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        rg_fake.clone() as Arc<dyn crate::domain::rg_port::RbacRgRead>,
    );
    let scope = format!("/tenants/{t_missing}/resourceGroups/{rg1}");
    assert!(matches!(
        validator.validate_scope_exists(&ctx(), &scope).await,
        Err(ScopeError::ScopeNotFound {
            missing: MissingScopeEntity::Tenant { id },
            ..
        }) if id == t_missing
    ));
    assert_eq!(
        rg_fake.get_group_calls.load(Ordering::SeqCst),
        0,
        "get_group must not be called when the tenant is absent"
    );
}

/// 4.12 — U-52: invalid scope format propagates through `validate_scope_exists`
/// (end-to-end path — parser rejection already covered in Group 2).
#[tokio::test]
async fn validate_scope_exists_invalid_format() {
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(FakeRbacRgRead::default()),
    );
    assert!(matches!(
        validator.validate_scope_exists(&ctx(), "/foo/bar").await,
        Err(ScopeError::InvalidScopeFormat { scope }) if scope == "/foo/bar"
    ));
}

/// A malformed UUID in the tenant segment is rejected before any
/// external call.
#[tokio::test]
async fn validate_scope_exists_malformed_uuid() {
    let tenant_fake = Arc::new(FakeTenantResolverClient::with_chain(&[]));
    let validator = ScopeValidator::new(
        tenant_fake.clone() as Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        Arc::new(FakeRbacRgRead::default()),
    );
    assert!(matches!(
        validator
            .validate_scope_exists(&ctx(), "/tenants/not-a-uuid")
            .await,
        Err(ScopeError::InvalidScopeFormat { .. })
    ));
    assert_eq!(
        tenant_fake.total_calls(),
        0,
        "no external call on malformed UUID"
    );
}

// ---------------------------------------------------------------------------
// Group 5: get_ancestor_scopes (tasks 5.5–5.11)
// ---------------------------------------------------------------------------

/// 5.5 — Root scope returns `["/"]` with zero external calls.
#[tokio::test]
async fn get_ancestor_scopes_root() {
    let tenant_fake = Arc::new(FakeTenantResolverClient::with_chain(&[]));
    let rg_fake = Arc::new(FakeRbacRgRead::default());
    let validator = ScopeValidator::new(
        tenant_fake.clone() as Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        rg_fake.clone() as Arc<dyn crate::domain::rg_port::RbacRgRead>,
    );
    assert_eq!(
        validator.get_ancestor_scopes(&ctx(), "/").await,
        Ok(vec!["/".to_owned()])
    );
    assert_eq!(
        tenant_fake.total_calls(),
        0,
        "root must not call tenant resolver"
    );
    assert_eq!(rg_fake.get_group_calls.load(Ordering::SeqCst), 0);
}

/// 5.6 — U-53: 3-level hierarchy returns root-to-leaf chain in exact order.
/// Chain: root → T1 → T2 → T3; request T3.
/// Expected: `["/", "/tenants/{root}", "/tenants/11111111-1111-1111-1111-11111111aaaa", "/tenants/22222222-2222-2222-2222-22222222aaaa", "/tenants/T3"]`.
#[tokio::test]
async fn get_ancestor_scopes_three_level_hierarchy() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let t3 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1, t2, t3])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let scope = format!("/tenants/{t3}");
    let expected = vec![
        "/".to_owned(),
        format!("/tenants/{root}"),
        format!("/tenants/{t1}"),
        format!("/tenants/{t2}"),
        format!("/tenants/{t3}"),
    ];
    assert_eq!(
        validator.get_ancestor_scopes(&ctx(), &scope).await,
        Ok(expected)
    );
}

/// A direct child of root has a two-step chain.
/// Chain: root → T1; request T1.
/// Expected: `["/", "/tenants/{root}", "/tenants/11111111-1111-1111-1111-11111111aaaa"]`.
#[tokio::test]
async fn get_ancestor_scopes_direct_child_of_root() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let scope = format!("/tenants/{t1}");
    let expected = vec![
        "/".to_owned(),
        format!("/tenants/{root}"),
        format!("/tenants/{t1}"),
    ];
    assert_eq!(
        validator.get_ancestor_scopes(&ctx(), &scope).await,
        Ok(expected)
    );
}

/// 5.8 — Root tenant (single-entry chain): requesting the root tenant itself
/// returns `["/", "/tenants/{R}"]` — root synthetic `/` plus the tenant scope.
#[tokio::test]
async fn get_ancestor_scopes_root_tenant() {
    let r = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[r])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let scope = format!("/tenants/{r}");
    let expected = vec!["/".to_owned(), format!("/tenants/{r}")];
    assert_eq!(
        validator.get_ancestor_scopes(&ctx(), &scope).await,
        Ok(expected)
    );
}

/// 5.9 — U-54: RG scope appends the RG path to the parent-tenant chain.
/// Chain: root → T1; request `/tenants/T1/resourceGroups/RG1`.
/// Expected: `["/", "/tenants/{root}", "/tenants/11111111-1111-1111-1111-11111111aaaa", "/tenants/11111111-1111-1111-1111-11111111aaaa/resourceGroups/30000000-0000-0000-0000-000000000003"]`.
/// Critically, `get_group` MUST NOT be called (RG hierarchy is flat).
#[tokio::test]
async fn get_ancestor_scopes_rg_scope() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let rg1 = Uuid::new_v4();
    let rg_fake = Arc::new(FakeRbacRgRead::default()); // no groups seeded — must stay at 0 calls
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1])),
        rg_fake.clone() as Arc<dyn crate::domain::rg_port::RbacRgRead>,
    );
    let scope = format!("/tenants/{t1}/resourceGroups/{rg1}");
    let expected = vec![
        "/".to_owned(),
        format!("/tenants/{root}"),
        format!("/tenants/{t1}"),
        format!("/tenants/{t1}/resourceGroups/{rg1}"),
    ];
    assert_eq!(
        validator.get_ancestor_scopes(&ctx(), &scope).await,
        Ok(expected)
    );
    assert_eq!(
        rg_fake.get_group_calls.load(Ordering::SeqCst),
        0,
        "get_group must never be called by get_ancestor_scopes"
    );
}

/// An unknown tenant returns `ScopeNotFound`.
#[tokio::test]
async fn get_ancestor_scopes_unknown_tenant() {
    let t_missing = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let scope = format!("/tenants/{t_missing}");
    assert!(matches!(
        validator.get_ancestor_scopes(&ctx(), &scope).await,
        Err(ScopeError::ScopeNotFound {
            missing: MissingScopeEntity::Tenant { id },
            ..
        }) if id == t_missing
    ));
}

// Note: 5.11 follows below — grouped together for readability.
/// 5.11 — RG-hierarchy-is-flat regression guard: two groups owned by the same
/// tenant must NOT appear as RG-level ancestors of each other.
/// Chain: root → T1; `RG_parent` and `RG_child` both owned by T1.
/// Request `/tenants/T1/resourceGroups/RG_child` → result MUST equal exactly
/// `["/", "/tenants/{root}", "/tenants/11111111-1111-1111-1111-11111111aaaa", "/tenants/11111111-1111-1111-1111-11111111aaaa/resourceGroups/RG_child"]`.
#[tokio::test]
async fn get_ancestor_scopes_rg_hierarchy_is_flat() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let rg_parent = Uuid::new_v4();
    let rg_child = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1])),
        // Seed both RGs owned by T1; neither should appear in the ancestor chain.
        Arc::new(
            FakeRbacRgRead::default()
                .with_group(rg_parent, t1)
                .with_group(rg_child, t1),
        ),
    );
    let scope = format!("/tenants/{t1}/resourceGroups/{rg_child}");
    let expected = vec![
        "/".to_owned(),
        format!("/tenants/{root}"),
        format!("/tenants/{t1}"),
        format!("/tenants/{t1}/resourceGroups/{rg_child}"),
    ];
    let result = validator.get_ancestor_scopes(&ctx(), &scope).await.unwrap();
    assert_eq!(
        result, expected,
        "RG hierarchy must be flat \u{2014} no RG-level ancestors"
    );
    assert!(
        !result.contains(&format!("/tenants/{t1}/resourceGroups/{rg_parent}")),
        "RG_parent must not appear in the ancestor chain of RG_child"
    );
}

// ---------------------------------------------------------------------------
// Group 6: is_ancestor (tasks 6.2–6.13)
// ---------------------------------------------------------------------------

/// 6.2 — U-55 self-ancestry, tenant: returns `Ok(true)` with zero upstream calls.
#[tokio::test]
async fn is_ancestor_self_tenant() {
    let t1 = Uuid::new_v4();
    let tenant_fake = Arc::new(FakeTenantResolverClient::with_chain(&[t1]));
    let validator = ScopeValidator::new(
        tenant_fake.clone() as Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        Arc::new(FakeRbacRgRead::default()),
    );
    let scope = format!("/tenants/{t1}");
    assert_eq!(
        validator.is_ancestor(&ctx(), &scope, &scope).await,
        Ok(true)
    );
    assert_eq!(
        tenant_fake.is_ancestor_calls.load(Ordering::SeqCst),
        0,
        "self-ancestry must not call upstream is_ancestor"
    );
}

/// 6.3 — Self-ancestry, resource group.
#[tokio::test]
async fn is_ancestor_self_rg() {
    let t1 = Uuid::new_v4();
    let rg1 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let scope = format!("/tenants/{t1}/resourceGroups/{rg1}");
    assert_eq!(
        validator.is_ancestor(&ctx(), &scope, &scope).await,
        Ok(true)
    );
}

#[tokio::test]
async fn is_ancestor_self_root() {
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(FakeRbacRgRead::default()),
    );
    assert_eq!(validator.is_ancestor(&ctx(), "/", "/").await, Ok(true));
}

/// 6.5 — U-56 true ancestor across tenant hierarchy.
/// Chain: root → T1 → T2; T1 is ancestor of T2.
#[tokio::test]
async fn is_ancestor_true_ancestor() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1, t2])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let anc = format!("/tenants/{t1}");
    let desc = format!("/tenants/{t2}");
    assert_eq!(validator.is_ancestor(&ctx(), &anc, &desc).await, Ok(true));
}

/// 6.6 — U-57 unrelated tenants: disjoint subtrees share root but T1 ≠ T3 branch.
#[tokio::test]
async fn is_ancestor_unrelated_tenants() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let t3 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_disjoint_subtrees(&[
            &[root, t1],
            &[root, t3],
        ])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let anc = format!("/tenants/{t1}");
    let desc = format!("/tenants/{t3}");
    assert_eq!(validator.is_ancestor(&ctx(), &anc, &desc).await, Ok(false));
}

/// Root `/` is ancestor of any tenant scope, with zero upstream calls
/// (root short-circuit fires before delegation).
#[tokio::test]
async fn is_ancestor_root_is_ancestor_of_tenant() {
    let t1 = Uuid::new_v4();
    let tenant_fake = Arc::new(FakeTenantResolverClient::with_chain(&[t1]));
    let validator = ScopeValidator::new(
        tenant_fake.clone() as Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        Arc::new(FakeRbacRgRead::default()),
    );
    let desc = format!("/tenants/{t1}");
    assert_eq!(validator.is_ancestor(&ctx(), "/", &desc).await, Ok(true));
    assert_eq!(
        tenant_fake.is_ancestor_calls.load(Ordering::SeqCst),
        0,
        "root short-circuit must not call upstream is_ancestor"
    );
}

#[tokio::test]
async fn is_ancestor_root_is_ancestor_of_rg() {
    let t1 = Uuid::new_v4();
    let rg1 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let desc = format!("/tenants/{t1}/resourceGroups/{rg1}");
    assert_eq!(validator.is_ancestor(&ctx(), "/", &desc).await, Ok(true));
}

/// A child is NOT an ancestor of its parent.
/// Chain: root → T1 → T2; T2 is a descendant, not an ancestor, of T1.
#[tokio::test]
async fn is_ancestor_child_not_ancestor_of_parent() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1, t2])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let anc = format!("/tenants/{t2}");
    let desc = format!("/tenants/{t1}");
    assert_eq!(validator.is_ancestor(&ctx(), &anc, &desc).await, Ok(false));
}

#[tokio::test]
async fn is_ancestor_tenant_not_ancestor_of_root() {
    let t1 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let anc = format!("/tenants/{t1}");
    assert_eq!(validator.is_ancestor(&ctx(), &anc, "/").await, Ok(false));
}

/// 6.11 — Tenant scope IS an ancestor of its own RG scope (same-tenant
/// short-circuit). Also confirms `RbacRgRead::get_group` is never called.
#[tokio::test]
async fn is_ancestor_tenant_is_ancestor_of_own_rg() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let rg1 = Uuid::new_v4();
    let rg_fake = Arc::new(FakeRbacRgRead::default());
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1])),
        rg_fake.clone() as Arc<dyn crate::domain::rg_port::RbacRgRead>,
    );
    let anc = format!("/tenants/{t1}");
    let desc = format!("/tenants/{t1}/resourceGroups/{rg1}");
    assert_eq!(validator.is_ancestor(&ctx(), &anc, &desc).await, Ok(true));
    assert_eq!(
        rg_fake.get_group_calls.load(Ordering::SeqCst),
        0,
        "is_ancestor must never call get_group"
    );
}

/// A resource group is an ancestor of nothing but itself, across tenants
/// as well as within one. Resource groups are flat inside a tenant, so
/// there is no shape for which this could be `true` other than equality,
/// which is answered earlier.
///
/// The tenant resolver must not be consulted at all: reducing an RG to
/// its tenant id in order to ask the question is precisely the mistake
/// this pins. `assignable_scopes` enforcement relies on it — an RG entry
/// that answered `true` here would widen a role scoped to one group into
/// the whole subtree below the group's tenant.
#[tokio::test]
async fn is_ancestor_rg_is_ancestor_of_nothing_across_tenants() {
    let root = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let group = Uuid::new_v4();
    let tenant_fake = Arc::new(FakeTenantResolverClient::with_chain(&[root, parent, child]));
    let validator = ScopeValidator::new(
        tenant_fake.clone() as Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        Arc::new(FakeRbacRgRead::default()),
    );
    let anc = format!("/tenants/{parent}/resourceGroups/{group}");

    assert_eq!(
        validator
            .is_ancestor(&ctx(), &anc, &format!("/tenants/{child}"))
            .await,
        Ok(false),
        "an RG scope is not an ancestor of a tenant below its own tenant"
    );
    assert_eq!(
        validator
            .is_ancestor(
                &ctx(),
                &anc,
                &format!("/tenants/{child}/resourceGroups/{}", Uuid::new_v4())
            )
            .await,
        Ok(false),
        "nor of a resource group inside that tenant"
    );
    assert_eq!(
        tenant_fake.is_ancestor_calls.load(Ordering::SeqCst),
        0,
        "an RG ancestor is decided locally; it must cost no round-trip"
    );
}

/// A tenant the hierarchy does not know is `ScopeNotFound`, not
/// `Upstream`. The distinction is the whole difference between a 4xx
/// naming the scope and an opaque 500 telling the caller to retry
/// something that can never succeed, and `ScopeError`'s own contract
/// reserves `Upstream` for errors that are not "missing".
#[tokio::test]
async fn is_ancestor_missing_tenant_is_scope_not_found() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let gone = Uuid::new_v4(); // never seeded
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1])),
        Arc::new(FakeRbacRgRead::default()),
    );

    assert!(
        matches!(
            validator
                .is_ancestor(&ctx(), &format!("/tenants/{gone}"), &format!("/tenants/{t1}"))
                .await,
            Err(ScopeError::ScopeNotFound {
                missing: MissingScopeEntity::Tenant { id },
                ..
            }) if id == gone
        ),
        "a missing ancestor must be reported as the missing tenant"
    );
    assert!(
        matches!(
            validator
                .is_ancestor(&ctx(), &format!("/tenants/{t1}"), &format!("/tenants/{gone}"))
                .await,
            Err(ScopeError::ScopeNotFound {
                missing: MissingScopeEntity::Tenant { id },
                ..
            }) if id == gone
        ),
        "and so must a missing descendant"
    );
}

/// 6.12 — Malformed `potential_ancestor` returns `InvalidScopeFormat` even when
/// both inputs are byte-equal (parse-first ordering locks out byte-equality shortcut).
#[tokio::test]
async fn is_ancestor_malformed_ancestor_format_error() {
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(FakeRbacRgRead::default()),
    );
    assert!(matches!(
        validator.is_ancestor(&ctx(), "/foo/bar", "/foo/bar").await,
        Err(ScopeError::InvalidScopeFormat { scope }) if scope == "/foo/bar"
    ));
}

/// 6.13 — Malformed descendant returns `InvalidScopeFormat` carrying the
/// descendant string, even when `potential_ancestor` is valid.
#[tokio::test]
async fn is_ancestor_malformed_descendant_format_error() {
    let t1 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let anc = format!("/tenants/{t1}");
    assert!(matches!(
        validator.is_ancestor(&ctx(), &anc, "/foo/bar").await,
        Err(ScopeError::InvalidScopeFormat { scope }) if scope == "/foo/bar"
    ));
}

// ---------------------------------------------------------------------------
// Group 7: Cross-Cutting Behavioral Tests (tasks 7.1–7.8)
// ---------------------------------------------------------------------------

/// 7.1 — `BarrierMode::Ignore` for `get_ancestor_scopes`: a self-managed T1
/// barrier must NOT drop ancestors above it (root must appear).
/// Behavioral proof: with `BarrierMode::Respect`, T1 would stop traversal and
/// root would be absent; with `BarrierMode::Ignore` the full chain appears.
#[tokio::test]
async fn barrier_mode_ignore_get_ancestor_scopes() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1, t2]).with_self_managed(&[t1])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let result = validator
        .get_ancestor_scopes(&ctx(), &format!("/tenants/{t2}"))
        .await
        .unwrap();
    let expected = vec![
        "/".to_owned(),
        format!("/tenants/{root}"),
        format!("/tenants/{t1}"),
        format!("/tenants/{t2}"),
    ];
    assert_eq!(
        result, expected,
        "barrier at T1 must not drop root \u{2014} proves BarrierMode::Ignore was passed"
    );
}

/// 7.2 — `BarrierMode::Ignore` for `is_ancestor`: root IS an ancestor of T2
/// even when T1 (between root and T2) is self-managed.
/// Behavioral proof: with `BarrierMode::Respect` the barrier at T1 would return
/// `false`; `Ok(true)` proves `BarrierMode::Ignore` was passed to the fake.
#[tokio::test]
async fn barrier_mode_ignore_is_ancestor() {
    let root = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[root, t1, t2]).with_self_managed(&[t1])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let anc = format!("/tenants/{root}");
    let desc = format!("/tenants/{t2}");
    assert_eq!(
        validator.is_ancestor(&ctx(), &anc, &desc).await,
        Ok(true),
        "root must be ancestor of T2 through self-managed T1 \u{2014} proves BarrierMode::Ignore"
    );
}

/// 7.3 — Fail-fast on invalid format: neither the tenant resolver nor the RG
/// reader is invoked when the scope string is malformed.
#[tokio::test]
async fn fail_fast_invalid_format_no_external_calls() {
    let tenant_fake = Arc::new(FakeTenantResolverClient::with_chain(&[]));
    let rg_fake = Arc::new(FakeRbacRgRead::default());
    let validator = ScopeValidator::new(
        tenant_fake.clone() as Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        rg_fake.clone() as Arc<dyn crate::domain::rg_port::RbacRgRead>,
    );
    let _discarded = validator.validate_scope_exists(&ctx(), "/foo/bar").await;
    assert_eq!(
        tenant_fake.total_calls(),
        0,
        "no tenant resolver call on malformed scope"
    );
    assert_eq!(
        rg_fake.get_group_calls.load(Ordering::SeqCst),
        0,
        "no RG read call on malformed scope"
    );
}

/// 7.4 — `ScopeError::InvalidScopeFormat` carries the offending string verbatim.
#[tokio::test]
async fn scope_error_invalid_format_carries_offending_string() {
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(FakeRbacRgRead::default()),
    );
    match validator.validate_scope_exists(&ctx(), "/foo/bar").await {
        Err(ScopeError::InvalidScopeFormat { scope }) => {
            assert_eq!(scope, "/foo/bar");
        }
        other => unreachable!("expected InvalidScopeFormat, got {other:?}"),
    }
}

/// 7.5 — `ScopeError::ScopeNotFound` for a missing tenant carries both the
/// original scope string and the missing tenant UUID.
#[tokio::test]
async fn scope_error_not_found_tenant_carries_full_detail() {
    let t_missing = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[])),
        Arc::new(FakeRbacRgRead::default()),
    );
    let scope = format!("/tenants/{t_missing}");
    match validator.validate_scope_exists(&ctx(), &scope).await {
        Err(ScopeError::ScopeNotFound {
            scope: s,
            missing: MissingScopeEntity::Tenant { id },
        }) => {
            assert_eq!(s, scope, "scope string must be the original input");
            assert_eq!(
                id, t_missing,
                "missing UUID must match the requested tenant"
            );
        }
        other => unreachable!("expected ScopeNotFound::Tenant, got {other:?}"),
    }
}

/// 7.6 — `ScopeError::ScopeNotFound` for an ownership mismatch carries all
/// three UUIDs (`rg_id`, `claimed_tenant_id`, `actual_tenant_id`) and the scope string.
#[tokio::test]
async fn scope_error_not_found_ownership_mismatch_carries_all_uuids() {
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let rg1 = Uuid::new_v4();
    let validator = ScopeValidator::new(
        Arc::new(FakeTenantResolverClient::with_chain(&[t1])),
        Arc::new(FakeRbacRgRead::default().with_group(rg1, t2)),
    );
    let scope = format!("/tenants/{t1}/resourceGroups/{rg1}");
    match validator.validate_scope_exists(&ctx(), &scope).await {
        Err(ScopeError::ScopeNotFound {
            scope: s,
            missing:
                MissingScopeEntity::ResourceGroupOwnerMismatch {
                    rg_id,
                    claimed_tenant_id,
                    actual_tenant_id,
                },
        }) => {
            assert_eq!(s, scope);
            assert_eq!(rg_id, rg1);
            assert_eq!(claimed_tenant_id, t1);
            assert_eq!(actual_tenant_id, t2);
        }
        other => unreachable!("expected ResourceGroupOwnerMismatch, got {other:?}"),
    }
}

/// 7.7 — Non-not-found upstream error is wrapped in `ScopeError::Upstream`,
/// never reinterpreted as `ScopeNotFound`.
#[tokio::test]
async fn non_not_found_upstream_error_is_wrapped() {
    let t1 = Uuid::new_v4();
    let fake = FakeTenantResolverClient::with_chain(&[t1])
        .with_tenant_failure(TenantResolverError::Internal("simulated outage".into()));
    let validator = ScopeValidator::new(Arc::new(fake), Arc::new(FakeRbacRgRead::default()));
    let scope = format!("/tenants/{t1}");
    assert!(
        matches!(
            validator.validate_scope_exists(&ctx(), &scope).await,
            Err(ScopeError::Upstream(UpstreamScopeError::Tenant(_)))
        ),
        "non-not-found upstream error must produce Upstream, not ScopeNotFound"
    );
}

/// 7.8 — `SecurityContext` is forwarded verbatim to upstream clients.
/// Creates a context with a known `subject_id` and asserts the fake recorded
/// the same UUID, proving the validator did not substitute a different context.
#[tokio::test]
async fn security_context_forwarded_verbatim() {
    let t1 = Uuid::new_v4();
    let unique_subject = Uuid::new_v4();
    let tagged_ctx = SecurityContext::builder()
        .subject_id(unique_subject)
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("valid SecurityContext");

    let tenant_fake = Arc::new(FakeTenantResolverClient::with_chain(&[t1]));
    let captured = tenant_fake.last_ctx_subject_id.clone();
    let validator = ScopeValidator::new(
        tenant_fake as Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
        Arc::new(FakeRbacRgRead::default()),
    );

    let scope = format!("/tenants/{t1}");
    let _discarded = validator.validate_scope_exists(&tagged_ctx, &scope).await;
    let recorded = captured.lock().expect("lock poisoned").unwrap();
    assert_eq!(
        recorded, unique_subject,
        "fake must receive the same subject_id that was passed to validate_scope_exists"
    );
}
