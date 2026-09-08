//! Scriptable in-memory `RbacServiceClientV1` fake.
//!
//! Configurable and call-counting: returns Allowed / Denied / Error per the
//! test's configuration. Mirrors the `RecordingTypesRegistry` pattern.
//!
//! Usage:
//!
//! - `InMemoryRbacServiceClient::default()` — unconfigured. Calling
//!   `evaluate_permission` returns a loud `Err(RbacServiceError::internal(...))`
//!   so a test that reaches the policy step without configuring the fake
//!   gets an actionable error.
//! - `InMemoryRbacServiceClient::with_allowed(grants, scope_hint)` — every
//!   `evaluate_permission` call derives a canonical `PermissionGranted` from
//!   the grants. The hint is used only to synthesize grants when none are given.
//! - `InMemoryRbacServiceClient::with_allowed_mismatched(grants, scope_type)` —
//!   returns an intentionally malformed aggregate for fail-closed tests.
//! - `InMemoryRbacServiceClient::with_denied(reason)` — every call returns
//!   `Ok(Denied(PermissionDenied))`.
//! - `InMemoryRbacServiceClient::with_error(rbac_error)` — every call
//!   returns `Err(rbac_error.clone())`.
//! - `fake.set_script(Script::*)` — change behavior between calls.
//! - `fake.call_count()` — total `evaluate_permission` invocations to date
//!   (used to assert "scope deny never calls RBAC").
//! - `fake.last_evaluate_permission_request()` — last request constructed by
//!   the plugin; tests assert on the precise SDK shape the plugin emits.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rbac_sdk::RbacServiceClientV1;
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{
    DenyReason, EffectivePermission, EvaluatePermissionRequest, EvaluatePermissionResponse,
    GetSubjectRolesRequest, GetSubjectRolesResponse, PermissionDenied, PermissionGranted,
    PermissionResult, PermissionRule, PermissionScopeType, Scope,
};

/// Script the fake follows for `evaluate_permission` calls.
#[derive(Debug, Clone)]
pub enum Script {
    /// Default — return a loud "configure me" error.
    DefaultStub,
    /// Return `Ok(Allowed(...))` with an aggregate canonically derived from
    /// the configured grants. `scope_type` is only a synthetic-grant hint when
    /// `grants` is empty; it is never copied into the response.
    Allowed {
        grants: Vec<EffectivePermission>,
        scope_type: PermissionScopeType,
    },
    /// Return an intentionally inconsistent/raw `Allowed` payload. Tests use
    /// this only to exercise consumer-side fail-closed behavior.
    AllowedMismatched {
        grants: Vec<EffectivePermission>,
        scope_type: PermissionScopeType,
    },
    /// Return `Ok(Denied(...))` with the configured deny reason.
    Denied(DenyReason),
    /// Return `Err(...)` with the configured RBAC error.
    Error(RbacServiceError),
}

pub struct InMemoryRbacServiceClient {
    script: Mutex<Script>,
    call_count: AtomicUsize,
    last_request: Mutex<Option<EvaluatePermissionRequest>>,
}

/// Build minimal assignment provenance for tests that care only about the
/// returned aggregate scope. The generated permission metadata is deliberately
/// inert: the fake has already decided to allow and no matcher consumes it.
fn synthetic_grants_for_scope(scope_type: &PermissionScopeType) -> Vec<EffectivePermission> {
    fn grant(scope: Scope) -> EffectivePermission {
        EffectivePermission::new(
            PermissionRule::new("read", "gts.cf.test.synthetic.resource.v1~"),
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            "Synthetic scoped test grant",
            scope,
            false,
        )
    }

    match scope_type {
        PermissionScopeType::Global => vec![grant(Scope::root())],
        PermissionScopeType::TenantSubtree { root_tenant_id } => {
            vec![grant(Scope::tenant(*root_tenant_id))]
        }
        PermissionScopeType::GroupSubtree { root_group_ids } => root_group_ids
            .iter()
            // `PermissionScopeType::GroupSubtree` intentionally carries only
            // group IDs. A nil tenant is sufficient for provenance because the
            // canonical classifier reads the group ID from the typed scope;
            // production assignments have already validated real ownership.
            .map(|group_id| grant(Scope::resource_group(uuid::Uuid::nil(), *group_id)))
            .collect(),
        PermissionScopeType::Combined { scopes } => {
            scopes.iter().flat_map(synthetic_grants_for_scope).collect()
        }
        // Reserved variants have no v1 assignment mapping. Canonical tests
        // therefore cannot synthesize them; consumer deny tests must opt into
        // `with_allowed_mismatched` explicitly.
        PermissionScopeType::TenantDirect { .. } | PermissionScopeType::ExplicitGroups { .. } => {
            Vec::new()
        }
        // `PermissionScopeType` is non-exhaustive. Future variants get no
        // invented provenance until tests teach the fake their assignment
        // mapping deliberately.
        _ => Vec::new(),
    }
}

// `RbacServiceClientV1` is a sealed trait; in-workspace implementors opt in
// explicitly. This test fake is such an implementor.
impl rbac_sdk::api::sealed::Sealed for InMemoryRbacServiceClient {}

impl Default for InMemoryRbacServiceClient {
    fn default() -> Self {
        Self {
            script: Mutex::new(Script::DefaultStub),
            call_count: AtomicUsize::new(0),
            last_request: Mutex::new(None),
        }
    }
}

impl InMemoryRbacServiceClient {
    /// Build a fake whose `evaluate_permission` returns a canonical
    /// `Ok(Allowed(...))`.
    ///
    /// Tests that exercise materialization rather than RBAC matching commonly
    /// pass an empty grant vector. For active scope variants the
    /// fake fills in minimal assignment provenance and derives the aggregate
    /// through [`PermissionGranted::from_grants`]. When callers provide grants,
    /// `scope_hint` is ignored so hand-written aggregate ordering cannot
    /// accidentally turn an unrelated test into a provenance-rejection test.
    #[must_use]
    pub fn with_allowed(grants: Vec<EffectivePermission>, scope_hint: PermissionScopeType) -> Self {
        let fake = Self::default();
        fake.set_script(Script::Allowed {
            grants,
            scope_type: scope_hint,
        });
        fake
    }

    /// Build a fake whose `evaluate_permission` returns the exact raw allow
    /// payload supplied by the caller, including empty or mismatched provenance.
    ///
    /// This constructor is intentionally explicit: production-shaped tests use
    /// [`Self::with_allowed`], while consumer hardening tests opt into malformed
    /// data without the fake silently rewriting their fixture.
    #[must_use]
    pub fn with_allowed_mismatched(
        grants: Vec<EffectivePermission>,
        scope_type: PermissionScopeType,
    ) -> Self {
        let fake = Self::default();
        fake.set_script(Script::AllowedMismatched { grants, scope_type });
        fake
    }

    /// Build a fake whose `evaluate_permission` returns `Ok(Denied(...))`.
    #[must_use]
    pub fn with_denied(reason: DenyReason) -> Self {
        let fake = Self::default();
        fake.set_script(Script::Denied(reason));
        fake
    }

    /// Build a fake whose `evaluate_permission` returns `Err(...)`.
    #[must_use]
    pub fn with_error(err: RbacServiceError) -> Self {
        let fake = Self::default();
        fake.set_script(Script::Error(err));
        fake
    }

    /// Swap the script between calls. Used by recovery tests that
    /// need RBAC to fail once and then start succeeding.
    pub fn set_script(&self, script: Script) {
        match self.script.lock() {
            Ok(mut guard) => *guard = script,
            Err(poisoned) => *poisoned.into_inner() = script,
        }
    }

    /// Total number of times `evaluate_permission` has been called on this
    /// fake. Used to assert "scope deny never calls RBAC" contracts.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// The last `EvaluatePermissionRequest` the plugin sent to this fake,
    /// or `None` if the fake has never been called. Used to assert on the
    /// precise shape (`subject_id` stringification, scope translation, etc.)
    /// the plugin emits.
    #[must_use]
    pub fn last_evaluate_permission_request(&self) -> Option<EvaluatePermissionRequest> {
        match self.last_request.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

#[async_trait]
impl RbacServiceClientV1 for InMemoryRbacServiceClient {
    async fn evaluate_permission(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        request: EvaluatePermissionRequest,
    ) -> Result<EvaluatePermissionResponse, RbacServiceError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);

        match self.last_request.lock() {
            Ok(mut guard) => *guard = Some(request),
            Err(poisoned) => *poisoned.into_inner() = Some(request),
        }

        let script = match self.script.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };

        match script {
            Script::DefaultStub => Err(RbacServiceError::internal(
                "test stub - configure InMemoryRbacServiceClient via with_allowed/with_denied/with_error",
            )),
            Script::Allowed { grants, scope_type } => {
                let grants = if grants.is_empty() {
                    synthetic_grants_for_scope(&scope_type)
                } else {
                    grants
                };
                let granted = PermissionGranted::from_grants(grants).map_err(|error| {
                    RbacServiceError::internal(format!(
                        "test fake could not derive allowed scope from grants: {error}"
                    ))
                })?;
                Ok(EvaluatePermissionResponse::from_result(
                    PermissionResult::Allowed(granted),
                ))
            }
            Script::AllowedMismatched { grants, scope_type } => {
                Ok(EvaluatePermissionResponse::from_result(
                    PermissionResult::Allowed(PermissionGranted::new(grants, scope_type)),
                ))
            }
            Script::Denied(reason) => Ok(EvaluatePermissionResponse::from_result(
                PermissionResult::Denied(PermissionDenied::new(reason)),
            )),
            Script::Error(err) => Err(err),
        }
    }

    async fn get_subject_roles(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _request: GetSubjectRolesRequest,
    ) -> Result<GetSubjectRolesResponse, RbacServiceError> {
        unreachable!(
            "InMemoryRbacServiceClient::get_subject_roles - \
             not exercised by any current test; extend the fake when one needs it"
        )
    }
}
