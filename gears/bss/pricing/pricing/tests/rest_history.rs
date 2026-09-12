//! Router-level tests for `GET /bss-pricing/v1/history` — Slice 12's first
//! reachable surface (§5, `inst-he-read`; D-12, D-125, D-270).
//!
//! The harness is `rest_frontier`'s, deliberately: both are unauthenticated-to-
//! authorized reads driven over a flat-`In` PDP, and two harnesses would be two
//! answers to what this gear's PEP returns. **The gates are not the same pair**,
//! and this paragraph said they were until 2026-08-20: `read_history` asks
//! `audit × read` and `export_history` asks `audit × export`
//! (`api/rest/history.rs`'s own gate note, since 2026-08-14 — the trail is its own
//! disclosure, so filing it under `plan × read` handed "who changed what, when"
//! to every holder of catalog read and left `audit_read` granting nothing).
//! A stale gate name in a test doc is how the wrong permission gets re-asserted.
//!
//! **What only a router test can see here** is the pair the engine cannot: that
//! the gate runs *before* the query, so a refusal is 403 and not an empty page;
//! and that a malformed `limit` or `cursor` reaches the wire as 400 rather than
//! as a 500 from a panic in parsing.
//!
//! Driven through the real router with `tower::ServiceExt::oneshot`, so the
//! extractors, the PEP gate and the response serialization are all in the path.
//! What the eight cases below drive — this list described the *frontier* route
//! until 2026-08-20, down to a `pin_eligible: false` reading and a
//! `pin_frontier_repo::advance` neither of which appears in this file:
//!
//!   (1) no authenticated `SecurityContext` ⇒ 401 — distinct from a denial,
//!       because the caller is not yet known;
//!   (2) the PDP denies ⇒ 403 `permission_denied`, not an empty page;
//!   (3) an authorized caller ⇒ 200 carrying one entry per price row with the
//!       `actor` taken from the row's own `created_by`, and `next_cursor` null
//!       when the walk is done;
//!   (4) `limit=0` and a malformed `cursor` ⇒ 400 each, named in the document;
//!   (5) the cursor the wire hands back opens the next page, and the walk is the
//!       commit order;
//!   (6) the page is filtered by the **compiled scope**, so a caller allowed
//!       only for another tenant reads none of its own rows;
//!   (7) `audit × read` alone reads and cannot export (403 on the export), which
//!       is what the second action buys;
//!   (8) the export chunk is the same walk the read serves.
//!
//! Every case runs against an in-memory `SQLite` migrated by the gear's own
//! `Migrator`, which is what makes it a test of the route and not of a mock: the
//! price rows are written through `PriceRepo` and read back across the
//! `SecureORM` tenant filter the compiled `AccessScope` binds.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bss_pricing::api::rest::history::{ApiState, router};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{NewPriceDraft, PriceRepo};

use sea_orm_migration::MigratorTrait;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_gts::gts_id;
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use tower::ServiceExt;
use uuid::Uuid;

const PATH: &str = "/bss-pricing/v1/history";
/// §5's export of the same trail (`inst-he-export`).
const EXPORT_PATH: &str = "/bss-pricing/v1/history/export";

/// Flat-`In` PDP fake: allows, and constrains `owner_tenant_id` to `allowed`.
/// This is the shape the real PDP returns for this gear (the PEP advertises no
/// tenant-subtree capability, so the subtree is pre-expanded to a flat `In`).
struct FlatInResolver {
    allowed: Vec<Uuid>,
}

#[async_trait]
impl AuthZResolverApi for FlatInResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        self.allowed.clone(),
                    ))],
                }],
                deny_reason: None,
            },
        })
    }
}

/// PDP fake that refuses. The route must surface this as 403, never as an empty
/// frontier: a consumer that reads "nothing to pin" stops, and it must stop for
/// the reason it was actually given.
struct DenyingResolver;

#[async_trait]
impl AuthZResolverApi for DenyingResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                constraints: vec![],
                deny_reason: Some(DenyReason {
                    error_code: "no_catalog_role".to_owned(),
                    details: Some("no catalog role grants plan x read".to_owned()),
                }),
            },
        })
    }
}

/// A PDP that grants **one action** on whatever resource is asked, and denies
/// every other.
///
/// The two fakes above cannot see an action at all — `FlatInResolver` allows
/// everything and `DenyingResolver` refuses everything — so neither can tell
/// `audit x read` from `audit x export`, which is the entire distinction §5 puts
/// between the two surfaces of this module. Without this the suite would be
/// silent about the one property the export route exists to have.
struct OnlyActionResolver {
    action: &'static str,
    allowed: Vec<Uuid>,
}

#[async_trait]
impl AuthZResolverApi for OnlyActionResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        if req.action.name == self.action {
            return Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext {
                    constraints: vec![Constraint {
                        predicates: vec![Predicate::In(InPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            self.allowed.clone(),
                        ))],
                    }],
                    deny_reason: None,
                },
            });
        }
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                constraints: vec![],
                deny_reason: Some(DenyReason {
                    error_code: "action_not_granted".to_owned(),
                    details: Some(format!(
                        "this role grants `{}` and not `{}`",
                        self.action, req.action.name
                    )),
                }),
            },
        })
    }
}

fn ctx_for(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(tenant)
        .subject_type(gts_id!("cf.core.security.subject_user.v1~"))
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("authed SecurityContext must build")
}

async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run the gear migrator");
    DBProvider::<DbError>::new(db)
}

/// The real router over a real reader, plus the layers `register_rest` applies —
/// **including the canonical-error middleware**, which this router omitted until
/// 2026-08-18 on the ground that these assertions were about status codes. That
/// ground was the finding rather than the reason: the suite asserted only statuses
/// because the body it was handed was not the body production serves.
///
/// No correlation layer: this is a read, and the edge travels with the mutating
/// routers only.
fn history_router(
    db: DBProvider<DbError>,
    enforcer: PolicyEnforcer,
    ctx: Option<SecurityContext>,
) -> Router {
    let state = Arc::new(ApiState {
        history: bss_pricing::infra::history::HistoryExporter::new(db),
    });
    let openapi = OpenApiRegistryImpl::new();
    let router = router(state, &openapi)
        .layer(axum::Extension(enforcer))
        .layer(axum::middleware::from_fn(
            toolkit::api::canonical_error_middleware,
        ));
    match ctx {
        Some(ctx) => router.layer(axum::Extension(ctx)),
        None => router,
    }
}

async fn get_at(router: Router, query: &str) -> axum::http::Response<Body> {
    router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("{PATH}{query}"))
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("send")
}

async fn post_at(router: Router, query: &str) -> axum::http::Response<Body> {
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{EXPORT_PATH}{query}"))
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("send")
}

/// The canonical **family** the problem document names.
///
/// `rest_support::problem_family`'s reading, re-typed for the same reason
/// `rest_sources` is re-typed in two test binaries: this suite builds its own
/// router and deliberately does not pull the shared harness in. Pinned to the
/// same `cf.core.err.*` id space by the assertions that call it — **including its
/// refusal**, which is the half the copy carried wrong until 2026-08-20.
async fn problem_family(response: axum::http::Response<Body>) -> String {
    let body = body_json(response).await;
    let raw = body["type"]
        .as_str()
        .unwrap_or_else(|| panic!("no canonical type in the problem document: {body}"))
        .to_owned();
    // `rsplit_once` and not `rsplit(..).next()`, because only the former can say
    // *no*: `rsplit` yields the whole string when the pattern is absent and
    // `split(..).next()` is always `Some`, so the diagnostic below used to be
    // unreachable and a `type` of `about:blank` came back as the "family"
    // `about:blank`. Same fix, same words as `rest_support::problem_family` — a
    // copy of a reading is a copy of its defects.
    let Some((_, tail)) = raw.rsplit_once("cf.core.err.") else {
        panic!("the canonical type is not a `cf.core.err.*` id: {raw}");
    };
    // The version suffix off, so a `~`-terminated GTS id yields the bare family.
    if let Some((family, _)) = tail.split_once(".v1") {
        family.to_owned()
    } else {
        tail.to_owned()
    }
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the body");
    serde_json::from_slice(&bytes).expect("the body is JSON")
}

/// One published price row, authored by `actor`, on its own market so nothing
/// collides on the canonical scope key.
async fn seed_row(db: &DBProvider<DbError>, tenant: Uuid, actor: Uuid, region: &str, hour: u32) {
    let prices = PriceRepo::new(db.clone());
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(9_900).expect("a non-negative amount"));
    let at = utc_ymd_hms(2026, 8, 8, hour, 0, 0);
    prices
        .create_draft(
            &AccessScope::for_tenant(tenant),
            tenant,
            NewPriceDraft {
                price_id: Uuid::now_v7(),
                scope_key: ScopeKey::new(
                    PlanId::new(Uuid::from_u128(0x9_1a4)),
                    CurrencyCode::new("EUR").expect("three letters"),
                    Region::new(region).expect("a non-blank region"),
                    PhaseId::new(Uuid::from_u128(0xfa_5e)),
                    PriceEligibility::AllSubscriptions,
                    ChargeKind::Recurring,
                    Cohort::None,
                )
                .expect("the class pairs with cohort none"),
                content: PriceContent {
                    row,
                    tax_inclusive: false,
                    tax_category_ref: None,
                    billing_timing: Some("advance".to_owned()),
                    proration_contract: None,
                    rounding_policy_ref: Some("half_up".to_owned()),
                    grandfather_until: None,
                    supersedes_price_id: None,
                },
                created_by: actor,
                created_at_utc: at,
                correlation_id: Uuid::now_v7(),
            },
        )
        .await
        .expect("author a row");
}

/// The happy path: a page in commit order, carrying the actor off the row's own
/// column.
#[tokio::test]
async fn a_page_of_history_carries_the_actor_from_the_row() {
    let tenant = Uuid::now_v7();
    let actor = Uuid::now_v7();
    let db = provider().await;
    seed_row(&db, tenant, actor, "eu", 10).await;

    let enforcer = PolicyEnforcer::new(Arc::new(FlatInResolver {
        allowed: vec![tenant],
    }));
    let response = get_at(history_router(db, enforcer, Some(ctx_for(tenant))), "").await;
    assert_eq!(response.status(), StatusCode::OK);

    let body = body_json(response).await;
    let entries = body["entries"].as_array().expect("entries is an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0]["actor"].as_str(),
        Some(actor.to_string().as_str()),
        "the actor is the row's own `created_by`, never the Auditor-only audit \
         log (D-12): {body}"
    );
    assert!(
        body["next_cursor"].is_null(),
        "a walk with nothing left hands back no cursor, so a client stops \
         without an extra round trip: {body}"
    );
}

/// **The gate runs before the query.** A refusal is 403 and not an empty page —
/// a caller who reads "no history" stops, and it must stop for the reason it was
/// actually given.
#[tokio::test]
async fn a_refused_caller_gets_403_and_not_an_empty_page() {
    let tenant = Uuid::now_v7();
    let db = provider().await;
    seed_row(&db, tenant, Uuid::now_v7(), "eu", 10).await;

    let enforcer = PolicyEnforcer::new(Arc::new(DenyingResolver));
    let response = get_at(history_router(db, enforcer, Some(ctx_for(tenant))), "").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(problem_family(response).await, "permission_denied");
}

/// No authenticated context is 401, and it is answered before anything reads.
#[tokio::test]
async fn an_unauthenticated_caller_gets_401() {
    let db = provider().await;
    let enforcer = PolicyEnforcer::new(Arc::new(FlatInResolver { allowed: vec![] }));
    let response = get_at(history_router(db, enforcer, None), "").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(problem_family(response).await, "unauthenticated");
}

/// **A malformed page request reaches the wire as 400, not as a 500.**
///
/// Both refusals are D-125's and both are the engine's: a zero page never
/// advances, and a cursor that does not decode names no position. What this case
/// adds over the engine's own is that the refusal survives the boundary with its
/// status intact rather than surfacing as an internal fault.
#[tokio::test]
async fn a_zero_limit_and_a_bad_cursor_are_both_400() {
    let tenant = Uuid::now_v7();
    let db = provider().await;
    let enforcer = PolicyEnforcer::new(Arc::new(FlatInResolver {
        allowed: vec![tenant],
    }));

    for query in ["?limit=0", "?cursor=not-a-token"] {
        let response = get_at(
            history_router(db.clone(), enforcer.clone(), Some(ctx_for(tenant))),
            query,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "`{query}` must be refused at the wire with 400"
        );
        assert_eq!(
            problem_family(response).await,
            "invalid_argument",
            "`{query}` is refused through the gear's ladder, not by axum's extractor"
        );
    }
}

/// The cursor round-trips through the wire: page one hands back a token, and
/// passing it as `cursor` opens page two on the row after the last.
#[tokio::test]
async fn the_cursor_the_wire_hands_back_opens_the_next_page() {
    let tenant = Uuid::now_v7();
    let db = provider().await;
    seed_row(&db, tenant, Uuid::now_v7(), "eu", 10).await;
    seed_row(&db, tenant, Uuid::now_v7(), "us", 11).await;

    let enforcer = PolicyEnforcer::new(Arc::new(FlatInResolver {
        allowed: vec![tenant],
    }));
    let first = body_json(
        get_at(
            history_router(db.clone(), enforcer.clone(), Some(ctx_for(tenant))),
            "?limit=1",
        )
        .await,
    )
    .await;
    assert_eq!(first["entries"].as_array().expect("entries").len(), 1);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a page with more behind it hands back a cursor")
        .to_owned();

    let second = body_json(
        get_at(
            history_router(db, enforcer, Some(ctx_for(tenant))),
            &format!("?limit=1&cursor={cursor}"),
        )
        .await,
    )
    .await;
    let entries = second["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    assert_ne!(
        entries[0]["price_id"], first["entries"][0]["price_id"],
        "the second page opens strictly after the first's last row: {second}"
    );
}

/// Default commit order is oldest-first; `$orderby=authored_at desc` reverses
/// and the cursor continues that order without repeating `$orderby`.
#[tokio::test]
async fn history_orderby_desc_reverses_and_the_cursor_continues_it() {
    let tenant = Uuid::now_v7();
    let db = provider().await;
    seed_row(&db, tenant, Uuid::now_v7(), "eu", 10).await;
    seed_row(&db, tenant, Uuid::now_v7(), "us", 11).await;

    let enforcer = PolicyEnforcer::new(Arc::new(FlatInResolver {
        allowed: vec![tenant],
    }));
    let all = body_json(
        get_at(
            history_router(db.clone(), enforcer.clone(), Some(ctx_for(tenant))),
            "",
        )
        .await,
    )
    .await;
    let entries = all["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2);
    let oldest = entries[0]["price_id"].clone();
    let newest = entries[1]["price_id"].clone();
    assert_ne!(oldest, newest);

    let desc = body_json(
        get_at(
            history_router(db.clone(), enforcer.clone(), Some(ctx_for(tenant))),
            "?$orderby=authored_at%20desc&limit=1",
        )
        .await,
    )
    .await;
    assert_eq!(
        desc["entries"][0]["price_id"], newest,
        "desc starts at the newest row: {desc}"
    );
    let cursor = desc["next_cursor"]
        .as_str()
        .expect("a desc page with more hands back a cursor")
        .to_owned();

    let next = body_json(
        get_at(
            history_router(db, enforcer, Some(ctx_for(tenant))),
            &format!("?limit=1&cursor={cursor}"),
        )
        .await,
    )
    .await;
    assert_eq!(
        next["entries"][0]["price_id"], oldest,
        "the cursor continues the desc walk: {next}"
    );
}

/// **The scope that filters is the PDP's, not the subject's tenant** — and this
/// case exists because a probe found nothing without it.
///
/// Replacing the compiled scope with `AccessScope::for_tenant(subject_tenant)`
/// changed no assertion in this file: the fake constrains to the same tenant the
/// context carries, so the two scopes were identical in every case. That made the
/// whole suite silent about which of them does the filtering — and the compiled
/// scope, with `require_constraints = true`, is the isolation.
///
/// Here the PDP allows but constrains to a **different** tenant. A route that
/// filtered on the subject's tenant would serve the caller's own rows; one that
/// filters on the compiled scope serves none.
#[tokio::test]
async fn the_page_is_filtered_by_the_compiled_scope_and_not_by_the_subject() {
    let caller = Uuid::now_v7();
    let elsewhere = Uuid::now_v7();
    let db = provider().await;
    seed_row(&db, caller, Uuid::now_v7(), "eu", 10).await;

    // Allowed, but constrained to a tenant the caller is not.
    let enforcer = PolicyEnforcer::new(Arc::new(FlatInResolver {
        allowed: vec![elsewhere],
    }));
    let body =
        body_json(get_at(history_router(db, enforcer, Some(ctx_for(caller))), "").await).await;
    assert_eq!(
        body["entries"].as_array().expect("entries").len(),
        0,
        "the compiled scope constrains to another tenant, so this caller's own \
         rows must not be served: {body}"
    );
}

/// **`audit x export` is a second permission, and this is what it buys.**
///
/// `inst-he-export` is a bulk walk of the whole >= 7-year trail; `actions::EXPORT`
/// exists so that extraction is grantable separately from reading
/// (`authz::actions::EXPORT`'s own doc). A role holding `audit x read` alone
/// therefore reads the history and cannot export it.
///
/// Both directions in one case, over one fixture and one grant, because either
/// alone proves nothing: a route gated on `read` would pass the 200 half, and a
/// route gated on nothing reachable would pass the 403 half.
#[tokio::test]
async fn a_role_granting_audit_read_alone_reads_the_history_and_cannot_export_it() {
    let tenant = Uuid::now_v7();
    let db = provider().await;
    seed_row(&db, tenant, Uuid::now_v7(), "eu", 10).await;

    let reader = PolicyEnforcer::new(Arc::new(OnlyActionResolver {
        action: "read",
        allowed: vec![tenant],
    }));

    let read = get_at(
        history_router(db.clone(), reader.clone(), Some(ctx_for(tenant))),
        "",
    )
    .await;
    assert_eq!(
        read.status(),
        StatusCode::OK,
        "the positive control: `audit x read` is what the interactive read asks for"
    );

    let export = post_at(
        history_router(db.clone(), reader, Some(ctx_for(tenant))),
        "",
    )
    .await;
    assert_eq!(
        export.status(),
        StatusCode::FORBIDDEN,
        "and the export asks for `audit x export`, which this role does not hold"
    );
    assert_eq!(problem_family(export).await, "permission_denied");

    // The mirror, so the 403 above is about the *action* and not about the route
    // being unreachable: the same caller with the export grant is served.
    let exporter = PolicyEnforcer::new(Arc::new(OnlyActionResolver {
        action: "export",
        allowed: vec![tenant],
    }));
    let granted = post_at(history_router(db, exporter, Some(ctx_for(tenant))), "").await;
    assert_eq!(granted.status(), StatusCode::OK);
}

/// `inst-he-export`: *"export streams the same commit order in bounded chunks"* —
/// the same walk, the same cursor, and a chunk the caller sizes.
///
/// Asserted against the **read's own** answer rather than against a literal, which
/// is what makes it a test of "the same order" rather than of "an order": the two
/// surfaces share `HistoryExporter` precisely so they cannot disagree, and a
/// second engine behind the export is the defect this pins.
#[tokio::test]
async fn the_export_chunk_is_the_same_walk_the_read_serves() {
    let tenant = Uuid::now_v7();
    let db = provider().await;
    seed_row(&db, tenant, Uuid::now_v7(), "eu", 10).await;
    seed_row(&db, tenant, Uuid::now_v7(), "us", 11).await;

    let enforcer = PolicyEnforcer::new(Arc::new(FlatInResolver {
        allowed: vec![tenant],
    }));
    let read = body_json(
        get_at(
            history_router(db.clone(), enforcer.clone(), Some(ctx_for(tenant))),
            "?limit=1",
        )
        .await,
    )
    .await;
    let chunk = body_json(
        post_at(
            history_router(db.clone(), enforcer.clone(), Some(ctx_for(tenant))),
            "?limit=1",
        )
        .await,
    )
    .await;
    assert_eq!(
        chunk["entries"], read["entries"],
        "one chunk of the export is the same records the read serves: {chunk}"
    );

    // GET mints an OData CursorV1; export still mints the history-engine token
    // (`infra::history::encode`). Same walk, different encodings — each token
    // must open the same next record; the bytes are not a shared contract.
    let export_cursor = chunk["next_cursor"]
        .as_str()
        .expect("a chunk with more behind it hands back a cursor")
        .to_owned();
    let read_cursor = read["next_cursor"]
        .as_str()
        .expect("a read page with more behind it hands back a cursor")
        .to_owned();
    let export_next = body_json(
        post_at(
            history_router(db.clone(), enforcer.clone(), Some(ctx_for(tenant))),
            &format!("?limit=1&cursor={export_cursor}"),
        )
        .await,
    )
    .await;
    let read_next = body_json(
        get_at(
            history_router(db, enforcer, Some(ctx_for(tenant))),
            &format!("?limit=1&cursor={read_cursor}"),
        )
        .await,
    )
    .await;
    assert_eq!(
        export_next["entries"][0]["price_id"], read_next["entries"][0]["price_id"],
        "each surface's token must open the same next record: export={export_next} read={read_next}"
    );
    assert_ne!(
        export_next["entries"][0]["price_id"], chunk["entries"][0]["price_id"],
        "and that next record is not the one already served: {export_next}"
    );
}
