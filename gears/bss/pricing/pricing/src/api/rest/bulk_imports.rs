//! The bulk import's three surfaces
//! (`design/12-operator-efficiency.md` §5, §4 `inst-bi-api`, `inst-bi-return`,
//! `inst-bs-abort`; D-118, D-290, D-291, D-292).
//!
//! # The two phases are one request, and that is what `202` is for
//!
//! `POST /bulk-imports` runs Phase 1 and, if it passes, Phase 2 — and answers
//! **202 with the operation ref** rather than the outcome. `inst-bi-return` says
//! so, and the reason survives the current implementation running both inline: a
//! batch is a long act, the run is a durable object with a report, and a caller
//! given a `202` and an id is a caller that does not have to hold the connection
//! open to learn what happened. `GET /bulk-imports/{id}` is where the answer
//! lives, for the first call and for every replay alike.
//!
//! A Phase-1 failure is the exception: `BULK_VALIDATION_FAILED`, and it is a
//! **400** — Foundation §3.3 gives the platform no 422 category, so every
//! architectural 422 reaches the wire as a 400 carrying its code. The refusal
//! carries a sentence and not the per-row report: the run is left
//! `validation_failed` holding it, and the `GET` is where a batch's answer lives
//! for every other outcome too.
//!
//! **A Phase-1 *store* fault lands on the same edge**. It is not a
//! validation refusal and does not answer 400 — the caller learns the storage
//! fault — but the run cannot be left where the fault found it: `validating` is a
//! state nothing can leave except this handler, so a run stranded there holds a
//! **spent** client key with no operator remedy at all. So the run is landed
//! `validation_failed` with the fault in its report before the error propagates,
//! and the replay arm then refuses instead of answering `202` for a run that
//! imported nothing.
//!
//! # The client key is the run's, not the gate's
//!
//! O4's replay here is **not** `idempotent::guarded`'s. That gate stores a
//! response body against a key with a TTL; a bulk run stores its own report on
//! its own row, unique per `(tenant, client_key)` by the table's index, and
//! `inst-bk-idem` replays *that* — "including during/after the lock window",
//! which outlives any dedup TTL. So the replay is a read of
//! `bulk_repo::find_by_client_key`, and a second `POST` under a key that already
//! opened a run answers the run it opened.
//!
//! **What the substitution cost, and what it costs now**. Replacing the
//! gate with the run's row dropped the gate's *payload* guard, and for a while
//! nothing here replaced it: the replay was `find_by_client_key` and nothing else,
//! so an operator who corrected a batch and resubmitted it under the same key was
//! answered `202` over the **first** batch's report, having imported nothing, with
//! no member of `BulkImportView` that could reveal it. The run now carries
//! `request_hash` (`pricing_bulk_operation`) — the same digest, on the same axis, as the
//! gate's own `pricing_idempotency_dedup.request_hash` — and a body that does not
//! match it is refused `IDEMPOTENCY_PAYLOAD_MISMATCH` rather than answered. So all
//! three axes the replay can be wrong on are closed: the state (D-295), the kind
//! (D-307) and the payload.
//!
//! # There is no row cap, and the absence is deliberate
//!
//! `rows` is a bare `Vec` and nothing on this path counts it. That is recorded
//! rather than fixed, because the two things a cap would need are both missing:
//!
//! * **No stated number on this axis.** The design set's nearest is §1.2's "500
//!   rows/plan", which D-160 settled as a **soft, advisory** publish-time warning
//!   (`PLAN_SIZE_SOFT_CAP_EXCEEDED`, on `warnings[]`, never blocking) — and it is
//!   per *plan*, while a batch spans plans (`pricing_bulk_operation` has a tenant
//!   and no plan). Refusing a request above it would both contradict the decision
//!   that made it advisory and measure the wrong axis. D-125, the read side's hard
//!   cap of 1,000, is a **pagination** decision about a response and says nothing
//!   about a request.
//! * **The cost is real but bounded by bytes.** Both phases run inline, each row is
//!   its own transaction, and every row takes a row lock — so a large batch does
//!   hold a connection open and rows frozen against interactive editors for as long
//!   as it runs. What bounds it today is the platform's request-body limit on the
//!   extractor this handler reads through: a body past it is refused before any
//!   handler code runs, so no run is opened and the idempotency key is not spent.
//!   `a_body_past_the_platform_limit_is_refused_before_the_handler_runs` pins that,
//!   because a bound named in prose and never measured is how "unbounded" came to
//!   be written here in the first place. It is also the **only** bound: this
//!   gear mounts no timeout layer, so the handler carries no deadline of its own.
//!
//! Moving Phase 2 off the request future is the other shape, and it is a design
//! decision rather than a refactor. The route already answers `202` with the run
//! ref and `GET /bulk-imports/{operationId}` already serves the outcome, so the
//! wire contract would survive it — but the gear has no background seam a route
//! can hand work to. `CancellationToken` reaches `Module::serve` and is spent on
//! the three tickers; there is no per-request task to carry the commit, and
//! moving it would change when a run reaches a terminal state, which
//! `CommitLockGuard`, the recorded idempotent response and the abort route all
//! read.
//!
//! What is owed is a **number**, from the design set, on the row axis — and the
//! operator remedy for a batch that is too big already exists either way: the
//! abort route.
//!
//! # Abort is an edge, not a new state
//!
//! `inst-bs-abort`: `committing → completed_with_conflicts`, uncommitted rows
//! reported `not-attempted`, the lock cleared. §4 already has that edge, which is
//! why abort needs no state of its own — and why a crashed import cannot freeze
//! interactive authoring: the operator has a door.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::HeaderMap, http::StatusCode};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::plans::idempotency_key_param;
use crate::api::rest::preconditions;
use crate::api::rest::prices::{PriceContentView, ScopeKeyRequest, content_of, scope_key_of};
use crate::api::rest::state::AuthoringState;
use crate::domain::bulk::{BulkKind, BulkState};
use crate::domain::concurrency::RowVersion;
use crate::domain::error::DomainError;
use crate::domain::import::{ImportRow, classify};
use crate::domain::instant::format_rfc3339;
use crate::domain::scope_key::PlanId;
use crate::infra::bulk::commit_batch;
use crate::infra::import::classify_against_store;
use crate::infra::storage::repo::{NewBulkOperation, bulk_repo};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

const TAG: &str = "BSS Pricing Bulk Import";

/// `POST` — submit a batch.
pub const BULK_IMPORTS: &str = "/bss-pricing/v1/bulk-imports";

/// `GET` — the run and its per-row report.
pub const BULK_IMPORT: &str = "/bss-pricing/v1/bulk-imports/{operationId}";

/// `POST` — abort a stalled mid-commit run (D-37).
pub const BULK_IMPORT_ABORT: &str = "/bss-pricing/v1/bulk-imports/{operationId}/abort";

/// §5's batch-level refusal: Phase 1 blocked the import.
pub const BULK_VALIDATION_FAILED: &str = "BULK_VALIDATION_FAILED";

/// One submitted row.
///
/// `plan_id` is a **field** here and a path segment on the interactive surface,
/// because a batch may span plans — `pricing_bulk_operation` carries a tenant and
/// no plan. The key and content views are the interactive route's own, so a row
/// an operator can author one at a time is the row they can author a thousand of.
// `(request, response)` and not `(request)` alone, for `ScheduleWindowRequest`'s
// reason: the submit digests the **parsed** batch (so member order and
// whitespace cannot make an honest retry look like a different intent), and that
// needs the type to serialize. Every keyed create in this gear is declared the
// same way.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct BulkImportRowRequest {
    /// The plan this row prices.
    pub plan_id: Uuid,
    /// The six axes authored as a key; `plan_id` above is the seventh, and the
    /// usage pair is derived from `content` rather than authored here.
    pub scope_key: ScopeKeyRequest,
    /// The row's whole content.
    pub content: PriceContentView,
    /// The version of the draft this row edits, absent when it claims a new key.
    pub if_match: Option<u64>,
}

/// The submitted batch.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct BulkImportRequest {
    /// The rows, in the order the report will name them.
    pub rows: Vec<BulkImportRowRequest>,
}

/// A run, and whatever report it has reached.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct BulkImportView {
    /// The run's durable name.
    pub operation_id: Uuid,
    /// One of §4's seven states.
    pub state: String,
    /// The client key it was opened under.
    pub client_key: String,
    /// The report as it stands — Phase 1's violations, or Phase 2's
    /// `{committed, conflicted}`. Shape depends on how far the run got, which is
    /// what `state` is for.
    pub report: serde_json::Value,
    /// When the run ended, absent while it is still going.
    pub completed_at: Option<String>,
}

/// What this surface needs.
pub struct ApiState {
    /// The authoring plane's state — the provider, the price repository and the
    /// scope every read goes through. No new field: Phase 2's engine takes the
    /// provider and the repository, and the run's own statements are free
    /// functions.
    pub authoring: Arc<AuthoringState>,
}

fn run_view(run: &crate::infra::storage::repo::BulkOperationRecord) -> BulkImportView {
    BulkImportView {
        operation_id: run.operation_id,
        state: run.state.as_str().to_owned(),
        client_key: run.client_key.clone(),
        report: run.report.clone(),
        completed_at: run.completed_at.map(format_rfc3339),
    }
}

/// Build the router for the three bulk-import surfaces.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post(BULK_IMPORTS)
        .operation_id("bss_pricing.submit_bulk_import")
        .summary("Submit a bulk price import")
        .description(
            "Runs the two-phase import. Phase 1 validates the whole batch and refuses it if any \
             row is invalid, answering `400` `BULK_VALIDATION_FAILED` and committing nothing; \
             the per-row report is on the run, which the `GET` serves. Phase 2 then commits row by row under each row's own ETag: \
             a conflict fails only that row, committed rows stand, and the report lists the \
             conflicted for retry. The answer is `202` with the operation ref; the report is at \
             `GET /bss-pricing/v1/bulk-imports/{operationId}`. The import authors **drafts** \
             (D-118): a row aimed at a published row's key is refused per-row, its remedy being \
             a repricing run. Replaying the `Idempotency-Key` answers the run it opened, \
             including during and after the lock window.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(idempotency_key_param())
        .json_request::<BulkImportRequest>(openapi, "The rows to import.")
        .handler(submit_bulk_import)
        .json_response_with_schema::<BulkImportView>(
            openapi,
            StatusCode::ACCEPTED,
            "The run, with its state and whatever report it has reached.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        // The replay guard's own status. A spent `Idempotency-Key` resubmitted over
        // a changed batch is `IDEMPOTENCY_PAYLOAD_MISMATCH`, which the ladder maps
        // through `aborted(...)` to a 409 — the refusal that stops a corrected batch
        // being answered `202` over the first batch's report, having imported
        // nothing. Declared here as it is at the four sibling creates.
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(BULK_IMPORT)
        .operation_id("bss_pricing.read_bulk_import")
        .summary("Read a bulk import's per-row report")
        .description(
            "The run and its report, which is where the answer to a submitted batch lives. \
             Gates on `plan` x `read`: the report is plan and price data, and an operator who \
             may read prices may read what an import did to them.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("operationId", "The run to read.")
        .handler(read_bulk_import)
        .json_response_with_schema::<BulkImportView>(
            openapi,
            StatusCode::OK,
            "The run and its report.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(BULK_IMPORT_ABORT)
        .operation_id("bss_pricing.abort_bulk_import")
        .summary("Abort a stalled mid-commit run")
        .description(
            "Moves a `committing` run to `completed_with_conflicts`, clearing its row locks so \
             interactive authoring is not frozen by a run that stopped (D-37). Uncommitted rows \
             stay in the report as not attempted; committed rows stand, as they do on every \
             other exit from Phase 2. A run that is not `committing` is refused by the state \
             machine: this is an edge, not a new state.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("operationId", "The run to abort.")
        .param(idempotency_key_param())
        .handler(abort_bulk_import)
        .json_response_with_schema::<BulkImportView>(
            openapi,
            StatusCode::OK,
            "The aborted run, with its locks released.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // D-178's edge. Two of the three surfaces write — the submit authors rows and
    // an audit record per row, the abort moves the run — so every record a
    // request produces has to carry one correlation, not one each.
    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

/// `POST /bulk-imports`.
async fn submit_bulk_import(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let scope = write_scope(&enforcer, &ctx).await?;
    let client_key = preconditions::idempotency_key(&headers)?;
    // Parsed **after** the gate, this directory's stated discipline: a caller
    // outside the scope learns they are denied, not that their body is malformed.
    // `Json<T>` as an extractor would run before the handler and answer 415 or
    // 400 where a 403 is owed — which is what the authz census caught.
    let body: BulkImportRequest = preconditions::parse_body(&body)?;
    // The digest of what was *understood*, not of the bytes — `request_digest`'s
    // own argument: two bodies differing in member order or whitespace are one
    // request, and hashing the bytes would refuse an honest retry made through a
    // different HTTP client.
    let request_hash = preconditions::request_digest(&body)?;

    let conn =
        state.authoring.db.conn().map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!("bulk import: {e}")))
        })?;

    // **The replay is a read of the run, not of a dedup row.** `inst-bk-idem`
    // replays the report "including during/after the lock window", which outlives
    // any gate TTL — so the run's own unique `(tenant, client_key)` is the record.
    if let Some(existing) =
        bulk_repo::find_by_client_key(&conn, &scope, tenant, BulkKind::Import, &client_key)
            .await
            .map_err(|e| CanonicalError::from(repo_failure(&e)))?
    {
        // **A replay answers what the first call answered** (D-295). A batch
        // refused in Phase 1 was answered `400`, so a retry that answered `202`
        // would tell a client the resubmit succeeded where the original failed —
        // the one conclusion idempotency exists to prevent, and precisely the
        // client that retried on a timeout and cannot otherwise tell. The report
        // is replayed either way: the run holds it and the `GET` serves it.
        //
        // The message also states what the key now costs, because this is where
        // an operator meets it: the key is **spent on the run**, so a corrected
        // batch resubmitted under it would import nothing and be told nothing.
        //
        // The state covers two ways to get there — the rule path below, and a
        // Phase-1 *store* fault landed on the same edge. Both are "this key's batch
        // did not import", which is the fact the refusal is about; the run's report
        // is where the difference is written, and the `GET` serves it.
        if existing.state == BulkState::ValidationFailed {
            return Err(CanonicalError::from(DomainError::BulkValidationFailed {
                operation_id: existing.operation_id.to_string(),
                detail: "this key opened a batch that did not pass validation; the run holds \
                         what refused it, and a corrected batch is a new batch that needs its \
                         own idempotency key"
                    .to_owned(),
            }));
        }
        // **The payload guard, and it stands *after* the state one on purpose**. The replay
        // above was `find_by_client_key` and nothing else, so the body was never compared with
        // what the key first carried: a corrected batch resubmitted under a spent key was
        // answered `202` over the first batch's report, having imported nothing, with no member
        // of `BulkImportView` that could reveal the substitution. That is the inversion D-295
        // closed on the state axis and D-307 on the kind axis, on the third one.
        //
        // The order is what partitions the two refusals rather than stacking them.
        // `validation_failed` already answers a changed body with the remedy —
        // "a corrected batch is a new batch and needs its own idempotency key",
        // the sentence directly above — and answering that caller a payload
        // mismatch instead would replace a refusal naming their batch's fault with
        // one naming their key's. Every other state reaches here, and for those a
        // changed body has no refusal at all without this one: the run **did**
        // import, so `202` over its report is precisely the "your resubmit
        // succeeded" the guard exists to prevent.
        //
        // Empty is the one stored value no writer produces: `pricing_bulk_operation`
        // backfilled the runs that predate the column with it, and their bodies
        // are not recoverable from anywhere. Those replay as they did before this
        // guard existed — the alternative is refusing a legitimate retry of a run
        // whose payload nobody can verify, which spends the harm on the caller who
        // did nothing wrong.
        if !existing.request_hash.is_empty() && existing.request_hash != request_hash {
            return Err(CanonicalError::from(
                DomainError::IdempotencyPayloadMismatch(format!(
                    "idempotency key `{}` opened bulk import run {} over a different batch. A \
                     corrected or otherwise changed batch is a new batch and needs its own key; \
                     the run this key holds is at `GET {BULK_IMPORTS}/{}` and nothing of the \
                     batch just sent was imported",
                    existing.client_key, existing.operation_id, existing.operation_id
                )),
            ));
        }
        return Ok((StatusCode::ACCEPTED, Json(run_view(&existing))).into_response());
    }

    let now = OffsetDateTime::now_utc();
    let stamp = audit_stamp(&ctx, now, correlation);
    let run = bulk_repo::open(
        &conn,
        &scope,
        NewBulkOperation {
            operation_id: Uuid::now_v7(),
            tenant_id: tenant,
            kind: BulkKind::Import,
            client_key,
            request_hash,
            report: serde_json::json!({ "rows": [] }),
            submitted_by: ctx.subject_id(),
            submitted_at: now,
        },
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    // **The open is audited**, in a helper rather than
    // inline: this handler is at its cognitive-complexity ceiling, and the record
    // is one act with one failure reading.
    audit_the_open(
        &conn,
        &scope,
        tenant,
        &ctx,
        correlation,
        &run,
        body.rows.len(),
    )
    .await;

    // **Armed the instant the run exists**. From here to
    // Phase 2's `advance` nothing owned this run: a client disconnect or a panic
    // stopped at the next await inside `classify_against_store` and left it
    // `validating` — a state nothing else can leave — with its idempotency key
    // spent, so the retry was answered `202` over the placeholder report. Phase 2
    // has had a `Drop` guard for both exits since Z8-8; Phase 1 had none. Disarmed
    // at every point below where the run has left `validating` under its own
    // power.
    let mut validation_guard = crate::infra::bulk::ValidationGuard::new(
        state.authoring.db.clone(),
        scope.clone(),
        tenant,
        run.operation_id,
    );

    // **Phase 1 begins by reading the rows, and it reads all of them.** Run
    // *before* the run is opened and with a `?` per row, the first bad value aborts
    // the whole body with one `DomainError` carrying no index and no run —
    // "currency invalid: US" for a thousand-row batch, which is exactly the
    // round-trip-per-refusal the all-or-nothing posture exists to spare an operator
    // (`domain::import`: "a rule that can answer for a row **must** answer for every
    // row rather than stopping at the first").
    let rows = match rows_of(&body) {
        Ok(rows) => rows,
        Err(unreadable) => {
            // `refuse_unreadable_rows` lands the run itself.
            validation_guard.disarm();
            return Err(refuse_unreadable_rows(
                &conn,
                &scope,
                tenant,
                run.operation_id,
                &unreadable,
                body.rows.len(),
                now,
            )
            .await);
        }
    };

    // Phase 1: both halves, into one report.
    let mut report = classify(&rows);
    // **Not a bare `?`, and Z11-4 is why.** The run above is born `validating`, and
    // `validating` is a state nothing can leave except this handler: `abort` refuses
    // anything that is not `committing`, the lock table has no sweeper, and the only
    // other writers of a run's state are the rule path below and Phase 2. So a
    // store fault here used to strand the run there permanently — with the client
    // key **spent on it**, which makes the retry worse than the failure: the replay
    // arm above would answer `202` with the placeholder report, telling a client
    // that resubmitted on a timeout that its import succeeded, for a run that
    // imported nothing. That is the conclusion the D-295 refusal twenty lines up
    // exists to prevent, reproduced with no refusal at all.
    //
    // Phase 2 was hardened three times for exactly this hazard (D-294, D-297,
    // D-300) and Phase 1 not once, in the same handler; `domain::bulk::Rejected`
    // exists because D-267 found a run "stranded there with no exit".
    if let Err(fault) = classify_against_store(&conn, &scope, tenant, &rows, &mut report).await {
        let failure = repo_failure(&fault);
        // The report the batch had reached, plus why it stopped. A batch-level
        // member beside the per-row ones, the shape the abort's own note uses:
        // this fault is about the *read*, so it belongs to no row.
        let mut stored = stored_report(&report, run.operation_id);

        if let Some(object) = stored.as_object_mut() {
            object.insert("failure".to_owned(), serde_json::json!(failure.to_string()));
        }
        // Best-effort: if even this fails the run is left where it was, and the
        // caller still learns the original fault rather than the landing's — the
        // first is what they can act on.
        if let Err(landing) = bulk_repo::advance(
            &conn,
            &scope,
            tenant,
            run.operation_id,
            BulkState::Validating,
            BulkState::ValidationFailed,
            stored,
            now,
        )
        .await
        {
            tracing::error!(
                error = %landing,
                operation_id = %run.operation_id,
                "bss-pricing: a bulk import's Phase-1 store fault could not be recorded on the \
                 run; it is left validating, which has no exit, and its idempotency key is spent"
            );
        }
        // Landed (or logged as unlandable) just above either way, so the guard
        // has nothing left to do and would only race its own advance.
        validation_guard.disarm();
        return Err(CanonicalError::from(failure));
    }

    if report.blocks_the_batch() {
        let stored = stored_report(&report, run.operation_id);

        bulk_repo::advance(
            &conn,
            &scope,
            tenant,
            run.operation_id,
            BulkState::Validating,
            BulkState::ValidationFailed,
            stored,
            now,
        )
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
        validation_guard.disarm();
        // **A `400`, not the `422` §5 writes.** Foundation §3.3 states it once
        // for the whole design set: the platform has no 422 category, and every
        // architectural 422 reaches the wire as a 400 carrying its code. The
        // per-row report is not lost — the run holds it and the `GET` serves it,
        // which is where a caller reads a batch's answer anyway.
        return Err(CanonicalError::from(DomainError::BulkValidationFailed {
            operation_id: run.operation_id.to_string(),
            detail: format!(
                "{} of {} row(s) were refused before anything was committed; the run holds the \
                 per-row report",
                report.rows().len(),
                rows.len()
            ),
        }));
    }

    // Phase 2 owns the run from its own `advance` onwards, under `CommitLockGuard`.
    validation_guard.disarm();
    commit_batch(
        &state.authoring.db,
        &state.authoring.prices,
        &scope,
        tenant,
        run.operation_id,
        &rows,
        stamp,
        now,
    )
    .await
    .map_err(CanonicalError::from)?;

    let landed = bulk_repo::read(&conn, &scope, tenant, run.operation_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
        .ok_or_else(|| {
            CanonicalError::from(DomainError::Internal(
                "bulk import: the run vanished mid-request".to_owned(),
            ))
        })?;
    Ok((StatusCode::ACCEPTED, Json(run_view(&landed))).into_response())
}

/// The validation report as the `report` column holds it.
///
/// One spelling for both Phase-1 landings: a reader of the column parses an
/// object carrying `rows`, so a bare array on one failure path would be a second
/// document shape for the same column. The fallback is logged because it is
/// unreachable — `BatchReport` is plain data — and an unreachable branch that
/// fires silently leaves the run holding an empty report with nothing saying why.
fn stored_report(
    report: &crate::domain::import::BatchReport,
    operation_id: Uuid,
) -> serde_json::Value {
    serde_json::to_value(report).unwrap_or_else(|e| {
        tracing::error!(
            error = %e,
            operation_id = %operation_id,
            "bss-pricing: a bulk import's validation report would not serialize; the run lands \
             with an empty report"
        );
        serde_json::json!({ "rows": [] })
    })
}

/// `GET /bulk-imports/{operationId}`.
async fn read_bulk_import(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(operation_id): Path<Uuid>,
) -> Result<Json<BulkImportView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        // **`plan x read`, which is what the catalog always said.** S5 §3's
        // endpoint map files `GET /bss-pricing/v1/bulk-imports/{id}` under `plan
        // x read` and its bulk row files the whole plane under "the **same**
        // `plan x write` / `publish` -- bulk is authoring at scale... no new
        // authority". This handler asked for `historical_import x read` instead,
        // a *restricted* resource S5 §3 step 5 says is "never included in a
        // default role", so every principal the design set names for this
        // surface -- ProductManager, FinanceManager, CatalogAdmin -- was
        // answered 403 on a run they were entitled to read. The resource is
        // struck outright now (D-330), but the filing was wrong before the
        // strike: a run's journal is price data, and an operator who may read
        // prices may read what a run did to them.
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let conn =
        state.authoring.db.conn().map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!("bulk import: {e}")))
        })?;
    let run = bulk_repo::read(&conn, &scope, tenant, operation_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
        .ok_or_else(|| {
            CanonicalError::from(DomainError::NotFound {
                subject: "bulk import".to_owned(),
                id: operation_id.to_string(),
            })
        })?;
    Ok(Json(run_view(&run)))
}

/// `POST /bulk-imports/{operationId}/abort`.
async fn abort_bulk_import(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(operation_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<BulkImportView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Required, not optional, for D-178's reason: the audit record this route now
    // writes would otherwise carry no correlation id.
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let scope = write_scope(&enforcer, &ctx).await?;
    // Read for its refusal, not for a value: an abort with no key is a request the
    // operator cannot safely retry.
    preconditions::idempotency_key(&headers)?;

    let conn = state
        .authoring
        .db
        .conn()
        .map_err(|e| CanonicalError::from(DomainError::Internal(format!("bulk abort: {e}"))))?;
    let run = bulk_repo::read(&conn, &scope, tenant, operation_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
        .ok_or_else(|| {
            CanonicalError::from(DomainError::NotFound {
                subject: "bulk import".to_owned(),
                id: operation_id.to_string(),
            })
        })?;

    // **A run this operation already abandoned is answered, not refused** — the
    // half the required `Idempotency-Key` above was owed and never got.
    //
    // The key is read for its refusal and bound to nothing, so it opens no dedup
    // gate; what stood in for one was the state guard below, and that guard gave
    // exactly the wrong answer. The first abort moves the run out of `committing`,
    // so a client that timed out and retried — identical request, identical key —
    // was told `LIFECYCLE_FORBIDDEN` over an abort that had succeeded, which is the
    // conclusion the key exists to prevent.
    //
    // The discriminator is the note and not the state: `abandon_committing_run`
    // lands the run in `completed_with_conflicts`, the same state an ordinary
    // partially-conflicted import reaches, so the state alone cannot tell "this
    // operation ran" from "this run finished on its own". The note is written by
    // that sweep and by nothing else. That keeps the guard below whole — a
    // `completed_with_conflicts` run nobody aborted is still refused, for the
    // reason it states.
    //
    // The drop-guard's fallback stamps the same member with its own note, and that
    // is the right reading too: the locks are released and the run is terminal, so
    // an operator asking for an abort is asking for a state the run is already in.
    if run.state != BulkState::Committing
        && run.report.get(crate::infra::bulk::ABORTED_MEMBER).is_some()
    {
        return Ok(Json(run_view(&run)));
    }

    // **A `validating` run has a door too**. Phase 1 owns
    // no locks and has committed nothing, but `validating` is a state nothing else
    // can leave — the sweep below refuses it, there is no sweeper, and D-37's
    // lease takeover is unbuilt — so a run stranded there held its idempotency key
    // forever and the retry was answered `202` over an empty report. The Phase-1
    // drop guard covers the dropped future; this covers everything the guard
    // cannot, including a process killed outright, which is the residue
    // `CommitLockGuard`'s own doc says the abort route stays for.
    if run.state == BulkState::Validating {
        let landed = crate::infra::bulk::abandon_validating_run(
            &conn,
            &scope,
            tenant,
            operation_id,
            crate::infra::bulk::ABORT_VALIDATING_NOTE,
            OffsetDateTime::now_utc(),
        )
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
        audit_the_abort(&conn, &scope, tenant, &ctx, correlation, &landed).await;
        return Ok(Json(run_view(&landed)));
    }

    // **The trigger does not refuse this one, and D-293 claimed it did.** A move
    // to the state a run is already in returns early on both engines, so an abort
    // against a run already in `completed_with_conflicts` — the ordinary terminal
    // state of any partially-conflicted import — would rewrite `completed_at` and
    // stamp an abort note over a report where every row WAS attempted. §4 makes
    // abort an edge out of `committing`; a run that is over is refused here,
    // because the state machine cannot see the difference between a move and a
    // no-op.
    if run.state != BulkState::Committing {
        return Err(CanonicalError::from(DomainError::LifecycleForbidden(
            format!(
                "bulk operation {operation_id} is {}; abort acts on a run that is still \
                 committing, and a run that is over has no locks to clear and no work to stop",
                run.state.as_str()
            ),
        )));
    }

    // **The sweep is `infra::bulk`'s, not a spelling of its own**. Release
    // first then land terminal, the report added to rather than replaced, the
    // premise carried into the statement — all of it argued there, because the
    // dropped-commit guard performs the identical act and the ordering is
    // load-bearing on both paths. The state guard above survives it: it is what
    // gives an operator aborting a finished run the sentence naming the state,
    // where the store alone would answer a bare contention refusal.
    let aborted = crate::infra::bulk::abandon_committing_run(
        &conn,
        &scope,
        tenant,
        operation_id,
        crate::infra::bulk::ABORT_NOTE,
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    audit_the_abort(&conn, &scope, tenant, &ctx, correlation, &aborted).await;
    Ok(Json(run_view(&aborted)))
}

/// Record that an operator submitted a batch.
///
/// Nothing on this plane wrote an audit record at all: neither
/// `api::rest::bulk_imports` nor `infra::bulk` nor
/// `infra::storage::repo::bulk_repo` mentions `audit_repo`, while the sibling
/// repricing run writes a `BulkOperation`-kind `Create` for the same act — and
/// `AuditAction::Create`'s own doc names it. So a batch that Phase 1 refused, or
/// that the Phase-1 window swallowed, left no trace that anything was submitted.
///
/// Written on the same autocommit connection as the open rather than in one
/// transaction with it, which is a real and stated limitation: a fault landing
/// exactly between the two leaves a run with no record of its opening. The run
/// must be durable before Phase 1 starts — that is what makes the abort door and
/// the replay arm able to see it — and this plane holds no transaction to join.
/// The repricing run's `append` rides a `txn` because its caller already has one.
///
/// Best-effort: the run is open and the client key is spent, so refusing here
/// would strand exactly the run the Phase-1 guard exists to rescue, over a record
/// rather than over the import.
async fn audit_the_open(
    conn: &impl toolkit_db::secure::DBRunner,
    scope: &toolkit_db::secure::AccessScope,
    tenant: Uuid,
    ctx: &SecurityContext,
    correlation: Uuid,
    run: &crate::infra::storage::repo::BulkOperationRecord,
    row_count: usize,
) {
    if let Err(e) = crate::infra::storage::repo::audit_repo::append(
        conn,
        scope,
        crate::infra::storage::repo::NewAuditEntry {
            tenant_id: tenant,
            chain_id: crate::infra::storage::repo::audit_repo::bulk_operation_chain(
                run.operation_id,
            ),
            recorded_at: OffsetDateTime::now_utc(),
            actor_principal_id: ctx.subject_id(),
            action: crate::domain::audit::AuditAction::Create,
            subject_kind: crate::domain::audit::AuditSubjectKind::BulkOperation,
            subject_ref: crate::infra::storage::repo::audit_repo::bulk_operation_ref(
                run.operation_id,
            ),
            before_state: None,
            after_state: Some(serde_json::json!({
                "kind": BulkKind::Import.as_str(),
                "state": run.state.as_str(),
                "rowCount": row_count,
            })),
            approval_ref: None,
            correlation_id: correlation,
        },
    )
    .await
    {
        tracing::error!(
            error = %e,
            operation_id = %run.operation_id,
            "bss-pricing: a bulk import opened without its audit record"
        );
    }
}

/// Record who stopped a run, and from which state.
///
/// Nothing on this plane wrote an audit record at all, so `GET /audit` could not
/// answer "who aborted import X": the only trace was an `aborted` note on a
/// report the abort itself rewrote. The sibling repricing run writes a
/// `BulkOperation`-kind record for the same act.
///
/// `AuditAction::Abandon` and not `Delete`, for that variant's stated reason — the
/// row survives and the run keeps its identity. Best-effort for the same reason
/// the open's is: the sweep has already released the locks and landed the run, and
/// refusing here would leave an operator believing the abort failed when it is the
/// one thing that certainly succeeded.
async fn audit_the_abort(
    conn: &impl toolkit_db::secure::DBRunner,
    scope: &toolkit_db::secure::AccessScope,
    tenant: Uuid,
    ctx: &SecurityContext,
    correlation: Uuid,
    aborted: &crate::infra::storage::repo::BulkOperationRecord,
) {
    if let Err(e) = crate::infra::storage::repo::audit_repo::append(
        conn,
        scope,
        crate::infra::storage::repo::NewAuditEntry {
            tenant_id: tenant,
            chain_id: crate::infra::storage::repo::audit_repo::bulk_operation_chain(
                aborted.operation_id,
            ),
            recorded_at: OffsetDateTime::now_utc(),
            actor_principal_id: ctx.subject_id(),
            action: crate::domain::audit::AuditAction::Abandon,
            subject_kind: crate::domain::audit::AuditSubjectKind::BulkOperation,
            subject_ref: crate::infra::storage::repo::audit_repo::bulk_operation_ref(
                aborted.operation_id,
            ),
            before_state: None,
            after_state: Some(serde_json::json!({
                "kind": aborted.kind.as_str(),
                "state": aborted.state.as_str(),
            })),
            approval_ref: None,
            correlation_id: correlation,
        },
    )
    .await
    {
        tracing::error!(
            error = %e,
            operation_id = %aborted.operation_id,
            "bss-pricing: a bulk import was aborted without its audit record"
        );
    }
}

/// The `plan x write` gate both mutating surfaces take.
///
/// `resource_id = None` for `create_plan`'s reason, sharpened by D-281: a batch
/// spans plans and writes rows whose ids it mints, so there is no single resource
/// to name — and an id-shaped constraint could not filter these tables anyway.
///
/// **This sentence and the body disagreed until now, and the body was wrong.**
/// It asked for `historical_import x write`, which S5 §3 step 5 makes a
/// restricted grant "never included in a default role" — so a `ProductManager`, a
/// `FinanceManager` or a `CatalogAdmin` holding `plan x write` was refused the
/// submit and the abort alike. S5 §3's bulk row is unambiguous that no new
/// authority is involved: *"Bulk import / mass repricing / clone / bulk-import
/// abort … the **same** `plan x write` / `publish` — bulk is authoring at scale
/// (and abort is un-authoring at scale), no new authority"*. Slice 12's mass
/// repricing one route over already reads that way; this plane did not.
async fn write_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<toolkit_db::secure::AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(ctx.subject_tenant_id())),
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// The submitted body as the domain's rows.
///
/// **The usage line is derived here, through the store's own derivation** (D-306).
/// `ScopeKeyRequest` carries six axes and the pair `(meter, dimensionKey)` is
/// authored on the *content* (D-196 clause (3)), so a key built from the request
/// alone is line-less — and every Phase-1 rule that groups or looks up by key was
/// then comparing keys the store would never file these rows under.
///
/// Two consequences, both live before this: `duplicate_scope_keys` grouped two
/// rows differing **only** in their meter as one key and refused them as
/// duplicates, which its own sentence says they are not — *"those are two keys
/// and both author"*; and `draft_rows`' lookup could never match a metered draft,
/// so a bulk import could not edit one at all — the row took the create path and
/// came back `DUPLICATE_SCOPE_KEY`.
///
/// It goes through `price_repo::resolve_authored_usage_line` rather than a
/// spelling of its own, because a second derivation of the same pair is the
/// hazard the interactive plane's version already records: the store has one home
/// for the columns, so a disagreement between two derivations would survive
/// exactly as far as the first metered import.
/// # Every row answers, and every row's faults are named against it
///
/// The three derivations are run **independently** rather than chained through
/// `?`, because a row's key and its content fail separately and an operator
/// fixing one at a time is doing the work the batch exists to save them: a bad
/// currency is `scope_key_of`'s, a negative amount is `content_of`'s, and chained
/// through `?` a batch carrying both in one row hears about whichever comes
/// first. The usage
/// line is the one derivation that genuinely depends on the other two, so it is
/// attempted only when they both answered.
fn rows_of(body: &BulkImportRequest) -> Result<Vec<ImportRow>, Vec<UnreadableRow>> {
    let mut rows = Vec::with_capacity(body.rows.len());
    let mut unreadable: Vec<UnreadableRow> = Vec::new();
    for (index, row) in body.rows.iter().enumerate() {
        let mut faults: Vec<String> = Vec::new();
        let content = content_of(&row.content)
            .inspect_err(|e| faults.push(e.to_string()))
            .ok();
        let key = scope_key_of(PlanId::new(row.plan_id), &row.scope_key)
            .inspect_err(|e| faults.push(e.to_string()))
            .ok();
        let line = match (key, content.as_ref()) {
            (Some(key), Some(content)) => {
                crate::infra::storage::repo::price_repo::resolve_authored_usage_line(
                    &key,
                    &content.row,
                )
                .inspect_err(|e| faults.push(repo_failure(e).to_string()))
                .ok()
            }
            _ => None,
        };
        match (line, content) {
            (Some(scope_key), Some(content)) if faults.is_empty() => rows.push(ImportRow {
                scope_key,
                content,
                if_match: row.if_match.map(RowVersion::new),
            }),
            _ => unreadable.push(UnreadableRow { row: index, faults }),
        }
    }
    if unreadable.is_empty() {
        Ok(rows)
    } else {
        Err(unreadable)
    }
}

/// One submitted row that could not be read as a price row at all, with **every**
/// fault found against it.
///
/// A row rather than a key, for [`RowOutcome`](crate::domain::import::RowOutcome)'s
/// reason stated one module over: a row whose key is the problem cannot be named by
/// that key.
#[derive(Debug)]
struct UnreadableRow {
    /// Zero-based position in the submitted batch, the same coordinate Phase 1's
    /// own report uses.
    row: usize,
    /// Every fault, in derivation order. Never only the first.
    faults: Vec<String>,
}

/// Land the run on the Phase-1 refusal edge and answer `BULK_VALIDATION_FAILED`.
///
/// **These are Phase-1 row failures and they now answer like every other one.**
/// §5 is explicit that *any* Phase-1 failure is `BULK_VALIDATION_FAILED` with the
/// per-row report on the run, and that is what the route's own contract and its
/// `OpenAPI` registration say. Before this they escaped as whatever variant the
/// first bad value happened to raise — `CURRENCY_INVALID` or `AMOUNT_NEGATIVE`,
/// carrying no row index — from a point in the handler before any run existed, so
/// no run held a report for them at all and the `GET` had nothing to serve.
///
/// **The per-row codes are deliberately not minted.** Phase 1's report entries are
/// coded, and each code is declared in the design set
/// (`DUPLICATE_SCOPE_KEY`, `IMPORT_TARGETS_PUBLISHED`, `PRIMITIVE_RULES_UNBUILT`);
/// the design set names none for "this row is not a readable price row", and
/// inventing one here would put an undeclared code on a wire contract
/// `inst-bk-idem` replays. So these ride a member of their own beside `rows`, the
/// arrangement Z11-4's batch-level `failure` member already established, each entry
/// carrying its row and its sentences — which name the field and the value, because
/// the underlying refusals do.
///
/// The state landing is the rule path's: `validating → validation_failed`, with the
/// premise in the statement, so the replay arm above refuses instead of answering
/// `202` for a run that imported nothing. A failure to land it is logged and the
/// caller still learns the refusal — the first thing they can act on.
async fn refuse_unreadable_rows(
    conn: &impl toolkit_db::secure::DBRunner,
    scope: &toolkit_db::secure::AccessScope,
    tenant: Uuid,
    operation_id: Uuid,
    unreadable: &[UnreadableRow],
    submitted: usize,
    now: OffsetDateTime,
) -> CanonicalError {
    let stored = serde_json::json!({
        "rows": [],
        "unreadable": unreadable
            .iter()
            .map(|row| serde_json::json!({ "row": row.row, "faults": row.faults }))
            .collect::<Vec<_>>(),
    });
    if let Err(landing) = bulk_repo::advance(
        conn,
        scope,
        tenant,
        operation_id,
        BulkState::Validating,
        BulkState::ValidationFailed,
        stored,
        now,
    )
    .await
    {
        tracing::error!(
            error = %landing,
            operation_id = %operation_id,
            "bss-pricing: a bulk import's unreadable rows could not be recorded on the run; it \
             is left validating, which has no exit, and its idempotency key is spent"
        );
    }
    CanonicalError::from(DomainError::BulkValidationFailed {
        operation_id: operation_id.to_string(),
        detail: format!(
            "{} of {submitted} row(s) could not be read as a price row and nothing was \
             committed; the run holds which rows and every fault found against each of them",
            unreadable.len()
        ),
    })
}
