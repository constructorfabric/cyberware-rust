//! `/bss-pricing/v1/migrations` — scheduling, reading and cancelling a plan
//! migration (`inst-ms-api`, `inst-ms-return`, `inst-mg-cancel`, D-34).
//!
//! `api::rest::retirement`'s shape, and every argument that module makes about
//! this layer holds here verbatim and is not restated: why the body is parsed
//! **after** the gate, and why a mutating router carries D-178's correlation
//! edge.
//!
//! # What differs, and it is this module's own content
//!
//! **`DELETE` is a cancel, not a deletion.** §5 types the route `DELETE
//! /migrations/{id}` and D-34 rescopes what it means: the row is never removed,
//! it flips to `cancelled`. That is not a REST liberty — an executor re-reads the
//! schedule's state before each batch (`inst-mg-cancel`'s handshake), so a
//! deleted row would read as "no such migration" to the one party whose correct
//! behaviour is to **stop**. Absence and cancellation must not be one answer on
//! that lane, and the store refuses the `DELETE` statement outright to make sure
//! they never are.
//!
//! **The `202` is `inst-ms-return`'s and means what it says.** The catalog has
//! scheduled; nothing has migrated. Subscriptions creates the `PlanLink`s when
//! the effective date arrives and it calls `/start`. A `201` would claim a
//! resource whose whole content is a promise about the future.
//!
//! **A replay answers `200`, not `202`.** M2 makes the create idempotent on a
//! client-supplied `migration_id`, and a route that answered `202` to a retry
//! would tell a client it had just scheduled something it scheduled an hour ago.
//! The distinction is the only externally visible part of `inst-mg-idem`.
//!
//! **And a replay compares the request the key was spent on — all of it but the
//! instant.** `schedule_in`'s replay arm asks `migration_repo::StatedRequest`
//! (`source_plan_id`, `target_plan_id`, `scope`) before it returns the stored row,
//! so a resubmission under a spent `migrationId` naming a different plan pair or a
//! different subscriber scope is **409 `IDEMPOTENCY_PAYLOAD_MISMATCH`**, not a
//! `200`. `effective_at` is stated and deliberately **outside** the comparison, so
//! a resubmission that moves only the instant is still a replay and reads back the
//! stored schedule; two contract pins hold that, and whose decision it would be to
//! flip is recorded on `StatedRequest`.
//!
//! The body **is** compared: a resubmission naming a different pair, instant or
//! scope is not answered `200` with the stored schedule and its sender told nothing.
//! The narrower point stands, argued at [`crate::infra::migration::schedule_in`] —
//! this is the gear's only idempotent create that stores no request **digest**, so
//! the comparison is against the stored columns rather than against a record of the
//! body its requester sent. What this module owes is that the **published** contract
//! states what the door does; a description naming the wrong status is worse than a
//! stale comment, because clients are written against it.
//!
//! **The gate is `plan × migrate`.** Not `write` and not `retire`: §5 gives
//! migration its own action, and the authority to end a plan is not the authority
//! to move its subscribers onto another one.
//!
//! # Two routes of §5 are deliberately absent here
//!
//! `POST /migrations/{id}/start` and `/complete` — the D-65 Subscriptions
//! handshake. Their storage half is built and probed
//! (`migration_repo::start`/`complete`, persist-and-replay included), but the
//! routes are **not** mounted, because `/start` must run D-36's execution-time
//! re-resolution of the lock set and the boundary deltas, and that re-resolution
//! has no input in this system: the Contracts registry is absent and no
//! subscription is enumerable. A `/start` that returned an exclusion set computed
//! from nothing would hand Subscriptions a set it would honour as authoritative.
//! Left unmounted rather than mounted-and-lying; reported in the hand-back.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use bss_pricing_sdk::odata::MigrationFilterField;

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::api::rest::preconditions;
use crate::api::rest::state::GovernanceState;
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::domain::scope_key::PlanId;
use crate::infra::migration::ScheduleRequest;
use crate::infra::storage::repo::MigrationRecord;
use time::OffsetDateTime;

/// The collection route's registered path template.
///
/// The literal is repeated in the `OperationBuilder` call below because DE0801
/// validates a literal argument and silently passes a `const` one;
/// `tests/module_test.rs` binds the two spellings together.
pub const MIGRATIONS: &str = "/bss-pricing/v1/migrations";

/// The item route's registered path template.
pub const MIGRATION_BY_ID: &str = "/bss-pricing/v1/migrations/{migrationId}";

/// The `OpenAPI` tag this surface is filed under — the plan plane's, because the
/// subject is a pair of plans rather than a price row or a window.
const TAG: &str = "BSS Pricing Plans";

/// The body of a scheduling request.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct ScheduleMigrationBody {
    /// **Client-supplied, and the idempotency key** (M2, `inst-ms-api`).
    ///
    /// Required rather than minted server-side: a timed-out client must be able
    /// to retry and get the original schedule back, which is only possible if the
    /// client chose the identity. Mirrors Slice 12's `run_id`.
    pub migration_id: Uuid,
    /// The retiring side.
    pub source_plan_id: Uuid,
    /// The target. MUST be a published plan (`inst-mg-target`).
    pub target_plan_id: Uuid,
    /// When the migration takes effect. Validated against the tenant's notice
    /// period (D-49) with a 60-day floor.
    #[serde(with = "rfc3339")]
    pub effective_at: OffsetDateTime,
    /// `all` or a subscription filter. Free-form, because the catalog does not
    /// interpret it — it rides the `PlanMigrationScheduled` contract to the party
    /// that does.
    pub scope: Option<serde_json::Value>,
    // **`reason_code` was here and is removed**, for the reason its
    // twin on the retirement door was: required by the schema, discarded by the
    // handler, and claiming in its own doc to be "recorded on the audit trail"
    // that has no column for it. `schedule_migration` passed it nowhere.
}

/// One delta, rendered for the confirm screen.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DeltaView {
    /// The subscription it is about.
    pub subscription_ref: Uuid,
    /// `contract_locked` | `entitlement_overflow` | `addon_invalid_on_target` |
    /// `addon_missing_required` | `boundary_uncovered` | `leaves_grandfathered_row`.
    pub kind: String,
    /// Whether it would have stopped the schedule.
    pub blocking: bool,
}

/// A migration schedule, as the surfaces render it.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MigrationView {
    /// The client-supplied id.
    pub migration_id: Uuid,
    /// The retiring side.
    pub source_plan_id: Uuid,
    /// The revision the schedule was computed against.
    pub source_revision: u64,
    /// The published target.
    pub target_plan_id: Uuid,
    /// When it takes effect.
    #[serde(with = "rfc3339")]
    pub effective_at: OffsetDateTime,
    /// The instant D-49's notice period was measured from.
    #[serde(with = "rfc3339")]
    pub announced_at: OffsetDateTime,
    /// `scheduled` | `in_progress` | `completed` | `cancelled`.
    pub state: String,
    /// The schedule-time delta report, verbatim as stored.
    pub delta_report: serde_json::Value,
    /// The subscriptions excluded from the run — contract-locked, never broken.
    pub excluded_subscription_refs: Vec<Uuid>,
    /// **`true` when no subscription could be enumerated at all.**
    ///
    /// The catalog holds no subscriptions and the D-79 lane has no client, so an
    /// empty delta report means "nobody could be asked", not "nobody has a
    /// problem". Reported because those are opposite facts for an operator about
    /// to confirm, and a screen that showed an empty list without this flag would
    /// be showing an all-clear it has no basis for.
    pub subjects_unresolved: bool,
    /// **`true` when the contract-lock registry could not be asked**
    /// (`inst-cl-source`). Its absence is this system's only case, under which
    /// every subscription reads locked.
    pub exclusions_unresolved: bool,
}

impl MigrationView {
    fn of(record: &MigrationRecord) -> Self {
        let excluded = record
            .delta_report
            .get("excluded")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            migration_id: record.migration_id,
            source_plan_id: record.source_plan_id.get(),
            source_revision: record.source_revision,
            target_plan_id: record.target_plan_id.get(),
            effective_at: record.effective_at,
            announced_at: record.announced_at,
            state: record.state.as_str().to_owned(),
            delta_report: record.delta_report.clone(),
            excluded_subscription_refs: excluded,
            subjects_unresolved: record
                .delta_report
                .get("subjectsUnresolved")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            exclusions_unresolved: record
                .delta_report
                .get("locksUnresolved")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
        }
    }
}

async fn schedule_migration(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    correlation: Option<Extension<CorrelationId>>,
    body: axum::body::Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(correlation)?;
    let tenant = ctx.subject_tenant_id();

    // `plan x migrate`, an action of its own (§5).
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::MIGRATE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(tenant)),
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Parsed after the gate - `api::rest::supersessions`' house rule.
    let request: ScheduleMigrationBody = preconditions::parse_body(&body)?;
    let stamp =
        crate::api::rest::auth_context::audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);

    // Bounded before the call: the column is frozen by the table's append-only
    // trigger and the value is copied onto the `PlanMigrationScheduled` contract.
    let scope_json = request
        .scope
        .unwrap_or_else(|| serde_json::json!({ "kind": "all" }));
    crate::api::rest::require_bounded_scope(&scope_json)?;
    let scheduled = state
        .migrations
        .schedule(
            &scope,
            tenant,
            ScheduleRequest {
                migration_id: request.migration_id,
                source_plan_id: PlanId::new(request.source_plan_id),
                target_plan_id: PlanId::new(request.target_plan_id),
                effective_at: request.effective_at,
                scope_json,
            },
            stamp,
        )
        .await?;

    // A replay is a `200`: it scheduled nothing. See the module doc.
    let status = if scheduled.created {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(MigrationView::of(&scheduled.record))).into_response())
}

/// `GET /migrations`.
///
/// Gated `plan × read`, which is exactly what [`read_migration`] asks for and for
/// its stated reason — a schedule is a read of the authoring plane, not a
/// mutation of it. `resource_id` is `None` because there is no single resource to
/// name, so what the PDP compiles is the tenant filter the whole walk runs under.
///
/// It passes `owner_tenant_id: None`, which is [`read_migration`]'s shape one
/// function down and `authz::access_scope`'s stated rule for a read: the PDP
/// derives the scope from the subject and its role, never from a caller-supplied
/// tenant, and only a write passes `Some(target_tenant)` so the membership
/// assertion has a target to test.
///
#[allow(clippy::implicit_hasher)]
async fn list_migrations(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<MigrationView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant = ctx.subject_tenant_id();

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

    let page = state
        .migrations
        .list_odata(&scope, tenant, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page.items.iter().map(MigrationView::of).collect(),
        page_info: page.page_info,
    }))
}

async fn read_migration(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(migration_id): Path<Uuid>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant = ctx.subject_tenant_id();

    // `plan x read` (§5's endpoint map): the schedule and its delta report are a
    // read of the authoring plane, not a mutation of it.
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
        None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let record = state
        .migrations
        .load(&scope, tenant, migration_id)
        .await?
        .ok_or_else(|| {
            CanonicalError::from(DomainError::NotFound {
                subject: "migration".to_owned(),
                id: migration_id.to_string(),
            })
        })?;
    Ok((StatusCode::OK, Json(MigrationView::of(&record))).into_response())
}

async fn cancel_migration(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    correlation: Option<Extension<CorrelationId>>,
    Path(migration_id): Path<Uuid>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(correlation)?;
    let tenant = ctx.subject_tenant_id();

    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::MIGRATE,
        Some(crate::authz::OwnerTenant(tenant)),
        // **`None`, and not this object's own id.** The action is on a **plan** —
        // that is what the authz catalog's endpoint map puts it on — and the plan
        // id is not in hand here: it is resolved below, from a row this gate has
        // to authorize before it may be read. Passing the migration id asked the
        // PDP about an object the `plan` label carries no identity for, so a role
        // definition of the form "allow this action where `resource_id in
        // {{planA}}`" was evaluated against the wrong object and denied — the
        // availability arm of the rule `windows.rs` states at length and
        // restructured three handlers to keep.
        //
        // `None` is the tenant-wide question every batch gate in this gear already
        // asks, and the compiled scope still binds `tenant_id` in SQL, so nothing
        // widens. **What is still owed** is `windows.rs`'s coarse-then-narrow
        // pair — this ask to scope the lookup, then a second anchored ask on the
        // resolved plan — with its `FURTHER_QUESTIONS` row, which is what would
        // make the narrow authority a fact the census asserts rather than an
        // absence.
        /* resource_id */
        None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let stamp =
        crate::api::rest::auth_context::audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
    let cancelled = state
        .migrations
        .cancel(&scope, tenant, migration_id, stamp)
        .await?;

    // `200` and not `204`: the cancelled record is the answer. D-34 puts the
    // partial sets on it, and a body-less response would make an operator issue a
    // second call to learn what the run had already done.
    Ok((StatusCode::OK, Json(MigrationView::of(&cancelled))).into_response())
}

/// Build the Axum router for the migration surface and register its operations.
pub fn router(state: Arc<GovernanceState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get("/bss-pricing/v1/migrations")
        .operation_id("bss_pricing.list_migrations")
        .summary("List the tenant's plan migrations (cursor-paginated)")
        .description(
            "One page of the tenant's migration schedules in `migrationId` order, with an opaque \
             `cursor` and a `limit` whose server default is 100 and whose hard cap is 1,000 \
             (D-125). Narrow with `$filter=state eq 'scheduled'` (or `in_progress` / `completed` \
             / `cancelled`); omitting `$filter` returns every state, which is what an operator's \
             queue over pending **and** finished runs asks for - a completed run is the record \
             of what moved. Each entry carries the schedule-time delta report verbatim, exactly as \
             `GET /bss-pricing/v1/migrations/{migrationId}` does: it is frozen at schedule time, \
             so rendering it on a page costs no further read. Gates on `plan` x `read`, the pair \
             the by-id read already asks for - a schedule is a read of the authoring plane, not \
             a mutation of it.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Schedules per page (default 100, hard cap 1,000)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_migrations)
        .with_odata_filter::<MigrationFilterField>()
        .with_odata_orderby::<MigrationFilterField>()
        .json_response_with_schema::<Page<MigrationView>>(
            openapi,
            StatusCode::OK,
            "One page of the tenant's migration schedules.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    let router = OperationBuilder::post("/bss-pricing/v1/migrations")
        .operation_id("bss_pricing.schedule_migration")
        .summary("Schedule a migration of a plan's subscribers onto a published target")
        .description(
            "Schedules a migration (`inst-ms-api`). The catalog emits \
             `PlanMigrationScheduled`; **Subscriptions** creates the effective-dated `PlanLink`s \
             and executes - this gear never mutates a subscription and never re-opens a posted \
             period. \
             \
             `migrationId` is **client-supplied** and is the idempotency key (M2): a timed-out \
             retry returns the original schedule and answers `200`, never a second schedule and \
             never `202`. A fresh schedule answers `202` - the catalog has scheduled and nothing \
             has migrated yet. \
             \
             **A replay must carry the request the key was spent on.** A resubmission under \
             a spent `migrationId` naming a different source or target plan, or a different \
             subscriber scope, is a different migration rather than a retry and is refused \
             `409 IDEMPOTENCY_PAYLOAD_MISMATCH`; nothing is scheduled and the stored schedule \
             is left as it stands. \
             \
             `effectiveAt` is the one stated field **outside** that comparison: a \
             resubmission that changes only the instant is still a replay and is answered \
             `200` with the **stored** schedule, so read that body before treating a \
             corrected date as applied. The remedy for a mistyped schedule is `DELETE \
             .../migrations/{migrationId}` and a **new** id, not a re-post under the old one, and \
             that holds for a `cancelled` or `completed` id too: the key is spent for good. \
             \
             The target MUST be **published** (`MIGRATION_TARGET_INVALID`), must not be the source \
             plan itself, and `effectiveAt` must clear the tenant's configured notice period - \
             default floor **60 days**, D-49 - or the request is refused \
             `MIGRATION_NOTICE_TOO_SHORT` with the earliest admissible instant named. There is no \
             override on this request: a shorter migration needs an audited change to the notice \
             policy first. \
             \
             Blocking deltas (entitlement overflow, invalid or missing-required add-ons, a target \
             missing the subscriber's frozen `(currency, region)` row of matching frequency) \
             refuse the schedule with `MIGRATION_BLOCKED` and are enumerated; **an unresolved \
             blocking delta never persists a schedule**. Contract-locked subscriptions are \
             different: they are reported and **excluded**, never blocking, because the lock is \
             never broken. \
             \
             **Read `subjectsUnresolved` and `exclusionsUnresolved` before trusting an empty delta \
             report.** The catalog holds no subscriptions and the Contracts lock registry has no \
             client in this deployment, so an empty report means nobody could be enumerated rather \
             than nobody having a problem. \
             \
             Gates on `plan` x `migrate`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<ScheduleMigrationBody>(openapi, "The schedule to create.")
        .handler(schedule_migration)
        .json_response_with_schema::<MigrationView>(
            openapi,
            StatusCode::OK,
            "A replay under an idempotency key already spent: the existing schedule, \
             unchanged. Nothing was scheduled and nothing was enqueued.",
        )
        .json_response_with_schema::<MigrationView>(
            openapi,
            StatusCode::ACCEPTED,
            "The migration was scheduled; `PlanMigrationScheduled` is enqueued.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/bss-pricing/v1/migrations/{migrationId}")
        .operation_id("bss_pricing.read_migration")
        .summary("Read a migration schedule, its delta report and its progress")
        .description(
            "Returns the schedule, the **schedule-time** delta report verbatim, and the state it \
             stands in. The delta report is frozen at scheduling: D-36's execution-time \
             re-resolution lands in a separate exclusion snapshot and never overwrites the \
             evidence an operator confirmed against. \
             \
             Gates on `plan` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("migrationId", "The migration schedule.")
        .handler(read_migration)
        .json_response_with_schema::<MigrationView>(
            openapi,
            StatusCode::OK,
            "The migration schedule.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/bss-pricing/v1/migrations/{migrationId}")
        .operation_id("bss_pricing.cancel_migration")
        .summary("Cancel a scheduled or in-progress migration")
        .description(
            "**Cancels; it does not delete** (D-34, confirmed 2026-08-07). The row flips to \
             `cancelled` and is retained, because Subscriptions re-reads the schedule's state \
             before it begins execution and per processing batch thereafter - so a deleted row \
             would read as `no such migration` to the one party whose correct behaviour is to \
             stop. The store refuses a `DELETE` statement outright for the same reason. \
             \
             Both live states are cancellable. A `scheduled` run had attempted nothing; an \
             `in_progress` run is stopped part-way - further `PlanLink` processing halts and the \
             partial sets are recorded, while **already-migrated subscriptions are unaffected by \
             construction**, the catalog having never held their state. \
             \
             Only a **completed** run is uncancellable: `409 MIGRATION_COMPLETED`. That code \
             replaces the pre-D-34 `MIGRATION_ALREADY_EFFECTIVE`, which named the wrong fact - most \
             runs past their effective date can still be stopped. \
             \
             Answers `200` with the cancelled record rather than `204`, so the partial sets do not \
             need a second call to read. Gates on `plan` x `migrate`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("migrationId", "The migration schedule to cancel.")
        .handler(cancel_migration)
        .json_response_with_schema::<MigrationView>(
            openapi,
            StatusCode::OK,
            "The migration was cancelled; the record carries what it had already done.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
        .layer(Extension(state))
        // D-178's correlation edge, per-router as every mutating surface applies it.
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}
