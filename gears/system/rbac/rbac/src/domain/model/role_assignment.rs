//! Domain-internal projection of a `role_assignments` row.
//!
//! Lowering to [`rbac_sdk::models::RoleAssignment`] happens in
//! [`crate::api::service::lowering`].

use chrono::{DateTime, Utc};
use rbac_sdk::models::{PrincipalType, Scope};
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Domain representation of one `role_assignments` row.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleAssignmentModel {
    pub id: Uuid,
    pub role_definition_id: Uuid,
    pub principal_id: String,
    pub principal_type: PrincipalType,
    pub scope: Scope,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: String,
    /// Kind of the principal named by [`Self::created_by`], captured from
    /// the caller's `SecurityContext` when the row was written.
    ///
    /// `None` means "author identity not recorded", which happens for a row
    /// written before the author-identity columns existed, for a machine
    /// author with no user identity, and for a stored kind this binary does
    /// not recognise (parsed leniently on read so a display detail can never
    /// fail a list). Read-only: nothing updates the author of a row.
    pub created_by_type: Option<PrincipalType>,
    /// Home tenant of the principal named by [`Self::created_by`] — the
    /// tenant an identity reader must be asked to turn that subject id into
    /// a display name. `None` under the same conditions as
    /// [`Self::created_by_type`].
    ///
    /// Deliberately *not* derived from the row's scope: the author need not
    /// live in the tenant they granted a role in (a partner admin granting
    /// inside a child tenant is the normal case), so guessing would name the
    /// wrong person or nobody.
    pub created_by_tenant_id: Option<Uuid>,
}
