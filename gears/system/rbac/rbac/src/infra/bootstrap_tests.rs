//! Pure unit tests for `infra::bootstrap` (no database required).

use sea_orm::ActiveValue;

use crate::domain::builtin_roles_catalog::{CANONICAL_BUILTIN_ROLES, SYSTEM_CREATED_BY};

use super::*;

/// Read a `Set(_)` value or panic with a description.
macro_rules! read_set {
    ($field:expr) => {
        match &$field {
            ActiveValue::Set(v) | ActiveValue::Unchanged(v) => v.clone(),
            ActiveValue::NotSet => {
                panic!("active value MUST be Set: {}", stringify!($field))
            }
        }
    };
}

fn fixed_now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::<chrono::Utc>::UNIX_EPOCH
}

/// Every field of the bootstrap `ActiveModel` matches the spec's normative
/// values for the Owner-at-root assignment.
#[test]
fn bootstrap_creates_owner_at_root_when_subject_configured() {
    let subject_id = "user-test-1234";
    let now = fixed_now();
    let am = build_role_assignment_active_model(subject_id, now);

    assert_eq!(
        read_set!(am.role_definition_id),
        OWNER_ROLE_ID,
        "bootstrap MUST reference the canonical Owner UUID"
    );
    assert_eq!(
        read_set!(am.principal_id),
        subject_id,
        "bootstrap MUST copy the configured subject_id into principal_id verbatim"
    );
    assert_eq!(
        read_set!(am.principal_type),
        "User",
        "bootstrap MUST set principal_type = 'User' (spec 'Owner role assignment at root scope')"
    );
    assert_eq!(
        read_set!(am.scope),
        "/",
        "bootstrap MUST set scope = '/' (root scope)"
    );
    assert_eq!(
        read_set!(am.created_by),
        SYSTEM_BOOTSTRAP_CREATED_BY,
        "bootstrap MUST use the reserved 'system-bootstrap' attribution"
    );
    assert_eq!(
        read_set!(am.created_at),
        now,
        "created_at MUST be set to the supplied timestamp"
    );
    assert_eq!(
        read_set!(am.updated_at),
        now,
        "updated_at MUST equal created_at at insert time"
    );
    // scope_depth and tenant_id are derived from the root scope:
    // Scope::root().depth() == 1, Scope::root().tenant_id() == None.
    assert_eq!(
        read_set!(am.scope_depth),
        1,
        "bootstrap MUST set scope_depth = Scope::root().depth() (1 for root scope)"
    );
    assert!(
        matches!(am.tenant_id, ActiveValue::Set(None)),
        "bootstrap MUST set tenant_id = Scope::root().tenant_id() (None for root scope)"
    );
}

/// `None` subject id MUST decide `Skip` so a missing config value cannot
/// pick up a phantom admin id.
#[test]
fn evaluate_bootstrap_decision_returns_skip_when_subject_is_none() {
    let decision = evaluate_bootstrap_decision(None);
    assert!(
        matches!(decision, BootstrapDecision::Skip),
        "a None subject_id MUST produce BootstrapDecision::Skip, not Run"
    );
}

/// `Some` subject id MUST decide `Run` with the exact subject string.
#[allow(clippy::panic)]
#[test]
fn evaluate_bootstrap_decision_returns_run_when_subject_is_some() {
    let subject = "user-admin-abc";
    let decision = evaluate_bootstrap_decision(Some(subject));
    match decision {
        BootstrapDecision::Run(id) => assert_eq!(
            id, subject,
            "Run variant MUST carry the subject_id verbatim"
        ),
        BootstrapDecision::Skip => {
            panic!("a Some subject_id MUST produce BootstrapDecision::Run")
        }
    }
}

#[test]
fn system_bootstrap_constant_is_distinct_from_seeder_constant() {
    assert_ne!(
        SYSTEM_BOOTSTRAP_CREATED_BY, SYSTEM_CREATED_BY,
        "the bootstrap attribution 'system-bootstrap' MUST NOT equal the \
         seeder attribution 'system'"
    );
}

/// `OWNER_ROLE_ID` matches the Owner entry in `CANONICAL_BUILTIN_ROLES`.
/// Changing either without updating the other is a cross-deployment break.
#[test]
fn owner_role_id_constant_matches_catalog() {
    let catalog_owner = CANONICAL_BUILTIN_ROLES
        .iter()
        .find(|r| r.name == "Owner")
        .expect("Owner MUST be in CANONICAL_BUILTIN_ROLES");

    assert_eq!(
        OWNER_ROLE_ID, catalog_owner.id,
        "OWNER_ROLE_ID constant MUST match the Owner entry in CANONICAL_BUILTIN_ROLES; \
         changing either without updating the other is a cross-deployment break"
    );
}

/// `CREDSTORE_SECRET_OPERATOR_ROLE_ID` matches the catalog entry. Changing
/// either without the other is a cross-deployment break.
#[test]
fn credstore_operator_role_id_constant_matches_catalog() {
    let catalog_role = CANONICAL_BUILTIN_ROLES
        .iter()
        .find(|r| r.name == "Credstore Secret Operator")
        .expect("Credstore Secret Operator MUST be in CANONICAL_BUILTIN_ROLES");

    assert_eq!(
        CREDSTORE_SECRET_OPERATOR_ROLE_ID, catalog_role.id,
        "CREDSTORE_SECRET_OPERATOR_ROLE_ID constant MUST match the catalog entry; \
         changing either without updating the other is a cross-deployment break"
    );
}

/// Every field of a configured grant `ActiveModel` matches the normative
/// values: the configured role and principal, at root scope, attributed to
/// the bootstrap writer.
#[test]
fn service_principal_grant_model_matches_spec() {
    let now = fixed_now();
    // Any opaque subject id; the point is that it is carried verbatim rather
    // than being a constant compiled into RBAC.
    let principal = "1d70b6d4-6e2e-4f3c-9aa3-7d8c2e3f5b91";
    let am = build_configured_grant_active_model(
        CREDSTORE_SECRET_OPERATOR_ROLE_ID,
        principal,
        PrincipalType::ServicePrincipal,
        now,
    );

    assert_eq!(
        read_set!(am.role_definition_id),
        CREDSTORE_SECRET_OPERATOR_ROLE_ID,
        "grant MUST reference the role it was given"
    );
    assert_eq!(
        read_set!(am.principal_id),
        principal,
        "grant MUST set principal_id verbatim from config"
    );
    assert_eq!(
        read_set!(am.principal_type),
        "ServicePrincipal",
        "grant MUST set principal_type = 'ServicePrincipal'"
    );
    assert_eq!(read_set!(am.scope), "/", "grant MUST be at root scope");
    assert_eq!(
        read_set!(am.created_by),
        SYSTEM_BOOTSTRAP_CREATED_BY,
        "grant MUST use the reserved 'system-bootstrap' attribution"
    );
}

/// The same builder writes a `User`-typed grant, which is the entire
/// difference between the two configured grant lists. A grant filed under the
/// wrong `principal_type` is never found by the evaluator — the request is
/// denied with nothing logged anywhere — so the type has to come from the
/// list the operator wrote, not from a field they could mistype.
#[test]
fn user_grant_model_differs_only_in_principal_type() {
    let now = fixed_now();
    let principal = "user-test-1234";
    let am =
        build_configured_grant_active_model(OWNER_ROLE_ID, principal, PrincipalType::User, now);

    assert_eq!(
        read_set!(am.principal_type),
        "User",
        "a user grant MUST be written as PrincipalType::User"
    );
    assert_eq!(read_set!(am.principal_id), principal);
    assert_eq!(read_set!(am.scope), "/", "grant MUST be at root scope");
    assert_eq!(
        read_set!(am.scope_depth),
        1,
        "root scope MUST record depth 1"
    );
    assert_eq!(
        read_set!(am.created_by),
        SYSTEM_BOOTSTRAP_CREATED_BY,
        "grant MUST use the reserved 'system-bootstrap' attribution"
    );
}
