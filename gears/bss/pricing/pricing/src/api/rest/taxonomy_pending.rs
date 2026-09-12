//! Read-only taxonomy proposal projection. Config access exposes pending
//! metadata; the full approval-read scope independently gates each preview.

use std::collections::{BTreeMap, BTreeSet};

use authz_resolver_sdk::PolicyEnforcer;
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::state::AuthoringState;
use crate::authz::{AuthzError, access_scope, actions, resource_types};
use crate::domain::approval::content_pin::taxonomy_value_content_hash;
use crate::domain::instant::rfc3339;
use crate::domain::taxonomy::{
    TaxCategoryPatch, TaxonomyClass, TaxonomyEntry, TaxonomyValueChange, TaxonomyValuePatch,
};
use crate::infra::storage::repo::approval_repo;
use crate::infra::storage::repo_failure;

/// Whether this caller may read this particular pending unit's proposal.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub enum PendingContentAccess {
    /// The full `approval × read` scope includes this unit.
    Granted,
    /// Only pending metadata is visible; this does not mean no proposal exists.
    Restricted,
}

/// Authored patch only. Omitted fields mean keep; an explicit category null clears.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TaxonomyProposedChangesView {
    /// Proposed label, without replacing the current value's label.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub display_name: Option<String>,
    /// Proposed `active` or `retired` state.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub state: Option<String>,
    /// Region only: absent keeps, null clears, a string sets the category.
    #[allow(
        clippy::option_option,
        reason = "preserve absent versus explicit null in the patch"
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tax_category: Option<Option<String>>,
    /// Proposed region rate-readiness marker, including an explicit false.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub tax_rate_present: Option<bool>,
}

impl From<&TaxonomyValuePatch> for TaxonomyProposedChangesView {
    fn from(patch: &TaxonomyValuePatch) -> Self {
        Self {
            display_name: patch.display_name.clone(),
            state: patch.state.map(|state| state.as_str().to_owned()),
            tax_category: match &patch.tax_category {
                TaxCategoryPatch::Keep => None,
                TaxCategoryPatch::Clear => Some(None),
                TaxCategoryPatch::Set(category) => Some(Some(category.clone())),
            },
            tax_rate_present: patch.tax_rate_present,
        }
    }
}

/// A submitted taxonomy unit. The first three fields match a plan pending link.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PendingTaxonomyApprovalView {
    /// The unit to open under `approval × read`.
    pub approval_id: Uuid,
    /// Always `taxonomy_value`.
    pub subject_kind: String,
    /// When the unit was opened, UTC.
    #[serde(with = "rfc3339")]
    pub submitted_at: OffsetDateTime,
    /// Explicit per-unit preview availability, not inferred from an empty array.
    pub content_access: PendingContentAccess,
    /// Present only for granted content; exactly the stored authored patch.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub proposed_changes: Option<TaxonomyProposedChangesView>,
    /// Present only for granted content. False means the current value plus
    /// proposal no longer matches the submitted pin, not a historical snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub content_matches_pin: Option<bool>,
}

/// Enrich already-authorized values, with at most two approval-table queries
/// and one approval-read PDP request for the whole collection.
///
/// The metadata join projects the config scope to tenant constraints only
/// after the exact values were read under its original scope. Content uses a
/// separate, unmodified approval scope. Denials restrict previews; PDP outages
/// fail with 503. No new authority or stored copy of a proposal is created.
pub(super) async fn for_values(
    state: &AuthoringState,
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    config_scope: &AccessScope,
    class: TaxonomyClass,
    entries: &[TaxonomyEntry],
) -> Result<BTreeMap<String, Vec<PendingTaxonomyApprovalView>>, CanonicalError> {
    let values: Vec<_> = entries
        .iter()
        .map(|entry| entry.value.as_str().to_owned())
        .collect();
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: taxonomy pending read: {e}")).create()
    })?;
    let pending = approval_repo::pending_for_taxonomy_values(
        &conn,
        &config_scope.tenant_only(),
        ctx.subject_tenant_id(),
        class,
        &values,
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let ids: Vec<_> = pending
        .values()
        .flatten()
        .map(|unit| unit.record.approval_id)
        .collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let readable = match access_scope(
        enforcer,
        ctx,
        &resource_types::APPROVAL,
        actions::READ,
        None,
        None,
    )
    .await
    {
        Ok(scope) => approval_repo::visible_ids(&conn, &scope, ctx.subject_tenant_id(), &ids)
            .await
            .map_err(|e| CanonicalError::from(repo_failure(&e)))?,
        Err(err @ AuthzError::Denied(_)) => {
            // Preserve the shared denial trace without failing the config read.
            let _denial = authz_error_to_canonical(err);
            BTreeSet::new()
        }
        Err(err @ AuthzError::Unavailable(_)) => return Err(authz_error_to_canonical(err)),
    };
    let mut views = BTreeMap::new();
    for entry in entries {
        let units = pending
            .get(entry.value.as_str())
            .into_iter()
            .flatten()
            .map(|unit| {
                let granted = readable.contains(&unit.record.approval_id);
                PendingTaxonomyApprovalView {
                    approval_id: unit.record.approval_id,
                    subject_kind: unit.record.subject_kind.as_str().to_owned(),
                    submitted_at: unit.record.submitted_at,
                    content_access: if granted {
                        PendingContentAccess::Granted
                    } else {
                        PendingContentAccess::Restricted
                    },
                    proposed_changes: granted
                        .then(|| TaxonomyProposedChangesView::from(&unit.proposal.patch)),
                    content_matches_pin: granted.then(|| {
                        unit.record.content_hash
                            == taxonomy_value_content_hash(&TaxonomyValueChange {
                                proposal: unit.proposal.clone(),
                                held: entry.clone(),
                            })
                    }),
                }
            })
            .collect();
        views.insert(entry.value.as_str().to_owned(), units);
    }
    Ok(views)
}
