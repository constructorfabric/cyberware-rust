//! `GET /bss-pricing/v1/migrated-origin-snapshots/{subscriptionRef}` — the
//! `migrated-origin` read surface (D-102, `inst-sy-surface`, `inst-sy-payload`,
//! `inst-sy-firstrating`).
//!
//! # Why this endpoint has to exist at all
//!
//! A `migrated-origin` ref resolves through **no** `CatalogVersion` by
//! construction (D-87, Foundation §4.4), so the read-model contract cannot
//! deliver it — the read model is keyed by version. Until D-102 the payload had
//! no reader-facing surface of any kind: it sat in `pricing_snapshot_provenance`,
//! S5's endpoint map covering "every REST surface of Slices 2–12" had none of it,
//! and none of the five §9.2 contracts carried it, while the PRD required
//! *"Rating/Tariffs evaluate **from that payload**"*. The rule existed and the
//! consumption mechanism did not.
//!
//! # The authz object is `plan` and the path object is a subscription
//!
//! Deliberately, and it is the one read surface in this gear where the two
//! differ. The gate is `plan × read` — the same authority the read model is
//! served under, because this *is* catalog content, just content no version
//! addresses. It is called by the Rating/Tariffs **service identity**.
//!
//! Because the two differ, **tenant binding is stated rather than inherited**:
//! the `subscriptionRef` resolves only within the caller's own `tenant_id`
//! through the Foundation §2.2 `SecureORM` filter. On every other read the path
//! object is itself the thing the scope filters, so row ownership comes for free;
//! here it does not, and D-102's own review fix (N-3) says so.
//!
//! # 404 before synthesis, and it is a contract rather than an accident
//!
//! `inst-sy-firstrating`: rating a legacy subscription **before** synthesis fails
//! closed into the exception path, synthesis runs as a separate audited step, and
//! rating retries against the frozen result. So a 404 here is the signal that
//! drives that flow — never a partial payload, never a guessed one. A caller that
//! got `200` with an incomplete body could not tell "not synthesized yet" from
//! "synthesized and this is what it costs".

use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::state::GovernanceState;
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::infra::storage::repo::ProvenanceRecord;
use time::OffsetDateTime;

/// The route's registered path template.
///
/// The literal is repeated in the `OperationBuilder` call below because DE0801
/// validates a literal argument and silently passes a `const` one.
pub const MIGRATED_ORIGIN_SNAPSHOT: &str =
    "/bss-pricing/v1/migrated-origin-snapshots/{subscriptionRef}";

/// The `OpenAPI` tag — the plan plane's, because the content is the catalog's
/// even though the path names a subscription.
const TAG: &str = "BSS Pricing Plans";

/// A frozen `migrated-origin` snapshot and its provenance.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MigratedOriginSnapshotView {
    /// The subscription.
    pub subscription_ref: Uuid,
    /// The record's own id.
    pub provenance_id: Uuid,
    /// The plan synthesis was about.
    pub source_plan_id: Uuid,
    /// The revision the resolved rows belonged to. **`null` is a fact, not a
    /// gap**, and since D-330 it is the live tier's own fact rather than the
    /// struck one's: every candidate is built from a `PriceWindow`, which carries
    /// no revision, so a resolution answers `null` here and D-87 obliges the
    /// self-contained payload either way.
    pub source_revision: Option<u64>,
    /// D-81's instant `t` — the migration effective timestamp, or the earliest
    /// unrated usage timestamp, depending on the trigger.
    #[serde(with = "rfc3339")]
    pub snapshot_instant: OffsetDateTime,
    /// `migration` | `first_rating`.
    pub trigger: String,
    /// Every resolved row id with the **selection tier** it came from — `source`,
    /// D-76 as narrowed by D-330, so an auditor can see which rule resolved the row
    /// without re-running the lookup.
    ///
    /// **`live_history` is the only value emitted.** D-76's second,
    /// `historical_import`, went with the historical-import flow (D-330), so promising
    /// it to clients here would name a value nothing produces. The field stays —
    /// dropping a provenance member on a seven-year record is a wire break with
    /// nothing bought, and S11 §4 clause 2 keeps it as the seam any later rule lands
    /// on.
    pub resolved: serde_json::Value,
    /// The self-contained payload (D-87 + C-5): the evaluable row content plus the
    /// plan-level descriptor set and grant set. Rating evaluates from this and
    /// Billing posts from it, resolving no id through the read model and no
    /// `CatalogVersion` at all.
    pub payload: serde_json::Value,
}

impl MigratedOriginSnapshotView {
    fn of(record: &ProvenanceRecord) -> Self {
        Self {
            subscription_ref: record.subscription_ref,
            provenance_id: record.provenance_id,
            source_plan_id: record.source_plan_id.get(),
            source_revision: record.source_revision,
            snapshot_instant: record.snapshot_instant,
            trigger: record.trigger.as_str().to_owned(),
            resolved: record.resolved.clone(),
            payload: record.payload.clone(),
        }
    }
}

async fn read_snapshot(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(subscription_ref): Path<Uuid>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant = ctx.subject_tenant_id();

    // `plan x read` — the same authority the read model is served under. The
    // resource id is **not** the subscription: the authz object is the plan
    // plane, and passing a subscription id as a `plan` resource would ask the PDP
    // about an object of the wrong type.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::READ,
        // **`None`, because this is a read** — `authz::access_scope`'s stated
        // two-way split: reads let the PDP derive the scope from the subject and
        // its role, never from a caller-supplied tenant, and only a write passes
        // `Some(target_tenant)` so the membership assertion has a target to test.
        // A read passing `Some(tenant)` runs that write-only assertion on a read.
        // Nothing escalates — the value is `ctx.subject_tenant_id()` and never
        // caller-supplied — but it is a live divergence between a module's stated
        // contract and its callers, and the contract is what a later reader trusts.
        /* owner_tenant_id */
        None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Tenant-bound through the scope, stated rather than inherited: this is the
    // one read whose path object is not the thing the filter keys on.
    let record = state
        .synthesis
        .load(&scope, tenant, subscription_ref)
        .await?
        .ok_or_else(|| {
            CanonicalError::from(DomainError::NotFound {
                subject: "migrated-origin snapshot".to_owned(),
                id: subscription_ref.to_string(),
            })
        })?;

    Ok((
        StatusCode::OK,
        Json(MigratedOriginSnapshotView::of(&record)),
    )
        .into_response())
}

/// Build the Axum router for the `migrated-origin` read surface.
pub fn router(state: Arc<GovernanceState>, openapi: &dyn OpenApiRegistry) -> Router {
    OperationBuilder::get("/bss-pricing/v1/migrated-origin-snapshots/{subscriptionRef}")
        .operation_id("bss_pricing.read_migrated_origin_snapshot")
        .summary("Read the frozen migrated-origin snapshot of a legacy subscription")
        .description(
            "Returns the frozen `migrated-origin` payload and its provenance for a subscription \
             that never had a `pricingSnapshotRef` (D-102). \
             \
             **This surface exists because nothing else can serve it.** A `migrated-origin` ref \
             resolves through **no** `CatalogVersion` by construction, so the read-model contract - \
             which is keyed by version - cannot deliver it. The payload is therefore \
             **self-contained** (D-87): the complete evaluable row content, plus the plan-level \
             billing descriptor set and grant set. Rating evaluates from it and Billing posts from \
             it, resolving no id through the read model. \
             \
             Each resolved id carries the **selection tier** it came from. D-76 declared two and \
             D-330 struck the second with the historical-import flow, so `live_history` - a \
             `pricing_price` row whose window covered the snapshot instant - is the only value \
             this surface can emit. The field is kept rather than dropped: it is provenance on a \
             record with a seven-year horizon, and an auditor reconstructing a disputed legacy \
             charge still reads which rule resolved the row without re-running the lookup. \
             \
             **404 before synthesis, and that is the contract.** Rating a legacy subscription \
             before its snapshot exists fails closed into the rating exception path; synthesis then \
             runs as a separate audited step and rating retries against the frozen result. This \
             endpoint never answers with a partial or guessed payload. \
             \
             Gates on `plan` x `read` and is called by the Rating/Tariffs **service identity**. The \
             `subscriptionRef` resolves only within the caller's own tenant.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "subscriptionRef",
            "The subscription whose migrated-origin snapshot to read.",
        )
        .handler(read_snapshot)
        .json_response_with_schema::<MigratedOriginSnapshotView>(
            openapi,
            StatusCode::OK,
            "The frozen payload and its provenance.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi)
        .layer(Extension(state))
}
