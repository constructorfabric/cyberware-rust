//! Gear-declaration smoke tests: the capability wiring is real, not decorative.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use bss_pricing::api::rest::state::{AuthoringState, GovernanceState};
use bss_pricing::config::LimitsConfig;
use bss_pricing::infra::approval::ApprovalService;
use bss_pricing::infra::fixture_gate::FixtureGate;
use bss_pricing::infra::publish::PublishService;
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, PinFrontierRepo, PlanRepo, PlanShapeRepo, PriceRepo,
};
use bss_pricing::infra::window::WindowService;
use bss_pricing::module::BssPricingGear;
use toolkit::GearCtx;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::api::operation_builder::ParamLocation;
use toolkit::contracts::{DatabaseCapability, RestApiCapability};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};

#[test]
fn the_gear_declares_the_database_capability() {
    // The `db` capability must resolve to the Foundation chain, not to an empty
    // vec: a gear that declares `db` and hands the platform nothing has tables
    // no migration ever creates.
    let gear = BssPricingGear::default();

    assert!(
        !gear.migrations().is_empty(),
        "the Foundation migration chain must be wired into the db capability"
    );
}

#[test]
fn every_migration_name_is_unique() {
    // The toolkit runner applies migrations in NAME order and rejects a
    // duplicate name outright — so a copy-pasted `DeriveMigrationName` does not
    // merely sort oddly, it aborts the whole chain at boot. Asserting it here
    // means the mistake is caught by `cargo test` rather than by a crash loop
    // in a cluster.
    let gear = BssPricingGear::default();
    let migrations = gear.migrations();
    let names: Vec<String> = migrations.iter().map(|m| m.name().to_owned()).collect();
    let unique: HashSet<&String> = names.iter().collect();

    assert_eq!(
        unique.len(),
        names.len(),
        "duplicate migration name in the chain: {names:?}"
    );
}

/// Every route this gear serves, spelled through the modules' own consts.
///
/// A path registered without a line here fails the census below, which is what
/// stops a route landing without anybody deciding it should — **as long as the
/// router that registers it is merged into `registered_operations`**, which is
/// hand-written and was the hole `rest_authz.rs`'s
/// `every_mounted_router_is_merged_into_both_censuses` now closes. The list is
/// also the only thing pinning those `const`s against the **literals** the
/// `OperationBuilder` calls take: DE0801 validates a literal argument and
/// silently passes a `const` one, so the route-shape rule binds where the
/// literal is, and this binds the two spellings together.
fn declared_paths() -> Vec<(&'static str, &'static str)> {
    use bss_pricing::api::rest::approvals::{
        APPROVAL, APPROVAL_APPROVE, APPROVAL_COUNTS, APPROVAL_REJECT, APPROVAL_WITHDRAW, APPROVALS,
    };
    use bss_pricing::api::rest::audit::AUDIT;
    use bss_pricing::api::rest::bulk_imports::{BULK_IMPORT, BULK_IMPORT_ABORT, BULK_IMPORTS};
    use bss_pricing::api::rest::bundles::{BUNDLE_BY_ID, BUNDLE_PUBLISH, BUNDLES};
    use bss_pricing::api::rest::catalog_skus::{CATALOG_SKUS, CATALOG_TAX_CATEGORIES};
    use bss_pricing::api::rest::customer_groups::{
        CUSTOMER_GROUP_MEMBER, CUSTOMER_GROUP_MEMBER_MOVE, CUSTOMER_GROUP_MEMBERS,
        CUSTOMER_GROUP_MEMBERS_MOVE, CUSTOMER_GROUP_TAXONOMY,
    };
    use bss_pricing::api::rest::cutovers::{PLAN_CUTOVERS, PRICE_GRANDFATHER_UNTIL};
    use bss_pricing::api::rest::frontier::{CATALOG_VERSION_REF, FRONTIER};
    use bss_pricing::api::rest::gl_codes::GL_CODES;
    use bss_pricing::api::rest::history::{HISTORY, HISTORY_EXPORT};
    use bss_pricing::api::rest::migrated_origin_snapshots::MIGRATED_ORIGIN_SNAPSHOT;
    use bss_pricing::api::rest::migrations::{MIGRATION_BY_ID, MIGRATIONS};
    use bss_pricing::api::rest::overlays::{
        PRICE_OVERLAY_BY_ID, PRICE_OVERLAY_SUBMIT, PRICE_OVERLAYS,
    };
    use bss_pricing::api::rest::plans::{PLAN, PLAN_ABANDON, PLAN_CLONE, PLANS, PLANS_COUNTS};
    use bss_pricing::api::rest::preview::PLAN_PREVIEW;
    use bss_pricing::api::rest::prices::{PLAN_PRICE, PLAN_PRICES};
    use bss_pricing::api::rest::publish::PLAN_PUBLISH;
    use bss_pricing::api::rest::repricing_runs::{
        REPRICING_RUN, REPRICING_RUN_ABORT, REPRICING_RUNS,
    };
    use bss_pricing::api::rest::retirement::PLAN_RETIRE;
    use bss_pricing::api::rest::rounding_policies::ROUNDING_POLICIES;
    use bss_pricing::api::rest::rounding_policy::ROUNDING_POLICY;
    use bss_pricing::api::rest::supersessions::PLAN_SUPERSESSIONS;
    use bss_pricing::api::rest::tax_display_policy::TAX_DISPLAY_POLICY;
    use bss_pricing::api::rest::taxonomies::{TAXONOMY, TAXONOMY_VALUE, TAXONOMY_VALUES};
    use bss_pricing::api::rest::threshold_policy::APPROVAL_THRESHOLD_POLICY;
    use bss_pricing::api::rest::windows::{
        PLAN_COVERAGE, PLAN_SELLABILITY, PRICE_WINDOW, PRICE_WINDOWS, PRICE_WINDOWS_LIST,
    };
    vec![
        ("GET", FRONTIER),
        ("GET", CATALOG_VERSION_REF),
        ("GET", HISTORY),
        // §5's export. A `POST` that is a **read** — `inst-he-nostore` leaves it
        // nothing to write — so it is here beside its sibling rather than among
        // the mutating rows, and `rest_authz`'s census carries it as
        // `mutating: false` for the same reason.
        ("POST", HISTORY_EXPORT),
        ("GET", AUDIT),
        ("GET", PLAN),
        ("GET", PLANS),
        // D-360: the counts read beside the list, a static segment ahead of
        // `{planId}`.
        ("GET", PLANS_COUNTS),
        ("POST", PLANS),
        ("PATCH", PLAN),
        ("POST", PLAN_ABANDON),
        ("POST", PLAN_CLONE),
        ("POST", BULK_IMPORTS),
        ("GET", BULK_IMPORT),
        ("POST", BULK_IMPORT_ABORT),
        // Slice 12's mass repricing (§5). Neither route declares a precondition
        // header: §5's Idempotency cell for the `POST` is `run_id`, which is a
        // body member, and `the_repricing_run_declares_no_precondition_header`
        // below is what keeps a later group from adding one to be helpful. The
        // `GET` is addressed by that same `run_id` rather than by the minted
        // operation id — see `api::rest::repricing_runs` for why.
        ("POST", REPRICING_RUNS),
        ("GET", REPRICING_RUN),
        // D-37's abort, the bulk import's one path over: `committing` is a state
        // `apply_run_in` leaves a stalled run in deliberately, and this is what
        // spends it. Addressed by the caller's `run_id` like the `GET` and never by
        // the minted operation id.
        ("POST", REPRICING_RUN_ABORT),
        ("POST", PLAN_PRICES),
        ("GET", PLAN_PRICES),
        ("GET", PLAN_PRICE),
        ("PATCH", PLAN_PRICE),
        ("DELETE", PLAN_PRICE),
        // Slice 8's three (`design/08-bundles.md` §5). The publish answers 202
        // per `inst-ba-return`, on that instruction's **event** half only: the
        // `BundleUpdated` the response does not wait for. Its read-model half is
        // unbuilt — a composition publish records no `PendingVersionRow`, so no
        // `CatalogVersion` advances and the composition never reaches a pin. See
        // `infra::bundle::publish_composition` and `domain::sellability`'s
        // `inst-sg-bundle` section for what that costs.
        // Slice 9's overlay half (`design/09-price-overlays.md` §5). The `PATCH`
        // is mounted per-resource rather than on the collection §5 spells,
        // because a precondition addresses a resource — the divergence Slice 8
        // reported for its own composition route and this one inherits. The
        // by-id `GET` is the authoring read (draft else published); create's
        // `Location` is that path. The submit answers 202: it opens the
        // always-material approval unit (D-50).
        ("POST", PRICE_OVERLAYS),
        ("GET", PRICE_OVERLAYS),
        ("GET", PRICE_OVERLAY_BY_ID),
        ("PATCH", PRICE_OVERLAY_BY_ID),
        ("POST", PRICE_OVERLAY_SUBMIT),
        // Slice 4's config plane: the four scope-value taxonomies, as one route
        // pair over a `{class}` segment (§5 writes the row as a single cell).
        // This is the surface that had never existed — `inst-plv-scope` and
        // `inst-tx-region` both validate against these tables, and until now the
        // only way to put a value in one was direct SQL.
        // Slice 4's base-price preview (§2, `inst-pv-api`). A read, gated on
        // `plan × preview` — deliberately not `plan × read`.
        ("GET", PLAN_PREVIEW),
        ("GET", TAXONOMY),
        // D-353: the whole-set `PUT` is gone — it could retire or re-label a value
        // without a second principal — and the per-value routes stand in its place.
        // The `POST` takes
        // **no** idempotency header — the value is the resource's natural key, so a
        // repeat with the same body replays and one with other content is `409`
        // `TAXONOMY_VALUE_EXISTS` — and is deliberately absent from
        // `idempotency_key_routes()` below, as `PLAN_SUPERSESSIONS` is. The `PATCH`
        // asserts the value's own tag and is in `if_match_routes()`.
        ("POST", TAXONOMY_VALUES),
        ("GET", TAXONOMY_VALUE),
        ("PATCH", TAXONOMY_VALUE),
        // Slice 9's own taxonomy (`inst-cg-taxonomy`), on its own route and its
        // own `customer_group` gate — see `api::rest::customer_groups`'s module
        // doc for why this is not a fifth arm of `TAXONOMY` above.
        ("GET", CUSTOMER_GROUP_TAXONOMY),
        ("PUT", CUSTOMER_GROUP_TAXONOMY),
        // Task 6: the membership routes, and the publish unit `dod-customer-group`'s
        // MUST requires — every committed membership mutation is its own publish
        // unit through the Foundation engine (D-06). Audit-only (`inst-mm-renewal`);
        // no approval unit opens.
        ("GET", CUSTOMER_GROUP_MEMBERS),
        ("POST", CUSTOMER_GROUP_MEMBERS),
        ("PATCH", CUSTOMER_GROUP_MEMBER),
        ("POST", CUSTOMER_GROUP_MEMBER_MOVE),
        ("POST", CUSTOMER_GROUP_MEMBERS_MOVE),
        ("GET", TAX_DISPLAY_POLICY),
        ("PUT", TAX_DISPLAY_POLICY),
        ("GET", ROUNDING_POLICY),
        ("PUT", ROUNDING_POLICY),
        ("GET", ROUNDING_POLICIES),
        ("PUT", ROUNDING_POLICIES),
        // D-356: the GL-code vocabulary, the third single-table taxonomy on the
        // `config` gate.
        ("GET", GL_CODES),
        ("PUT", GL_CODES),
        ("GET", CATALOG_SKUS),
        ("GET", CATALOG_TAX_CATEGORIES),
        ("POST", BUNDLES),
        ("GET", BUNDLES),
        // D-310: the composition's reader. It was unreadable through any surface,
        // including to the approver of the unit D-104 opens over it.
        ("GET", BUNDLE_BY_ID),
        ("PATCH", BUNDLE_BY_ID),
        ("POST", BUNDLE_PUBLISH),
        // Slice 7's two reads: the coverage report an operator remediates from,
        // and the gate's surface over one pinned delta. Two routes and not one
        // because they answer different questions over different row sets - see
        // the `api::rest::windows` module doc.
        ("GET", PLAN_COVERAGE),
        ("GET", PLAN_SELLABILITY),
        // Slice 7's mutating half: the window surfaces §5 declares, each a publish
        // unit (D-99) answering 202, and two of them under D-62's two-person control.
        // The `DELETE` carries no idempotency header, which is §5's own column and is
        // reported as a divergence in `api::rest::windows`.
        ("POST", PRICE_WINDOWS),
        ("GET", PRICE_WINDOWS_LIST),
        ("PATCH", PRICE_WINDOW),
        ("DELETE", PRICE_WINDOW),
        // Slice 7's interactive repricing: the supersession unit (D-88), one route in a
        // module of its own. It answers 202 on both arms and takes **no** idempotency
        // header, which is S5's own column for it — the act's identity is the key — so
        // it is deliberately absent from `idempotency_key_routes()` below and the
        // divergence its natural key leaves past the commit is reported in
        // `api::rest::supersessions`.
        ("POST", PLAN_SUPERSESSIONS),
        ("POST", PLAN_CUTOVERS),
        // Slice 7's horizon door (S7 §5, `inst-gs-bound`/`inst-gs-tighten`),
        // mounted beside the cutover in the same module because the two are one
        // mechanism read from its ends. Its §5 Idempotency cell is `ETag`, so it
        // is in `if_match_routes()` below and deliberately **not** in
        // `idempotency_key_routes()`: it is a mutation of a row that exists, not
        // a create.
        ("PATCH", PRICE_GRANDFATHER_UNTIL),
        ("POST", PLAN_RETIRE),
        // Slice 11's migration plane. `DELETE` is a **cancel** (D-34): the row
        // flips to `cancelled` and is never removed, because an executor
        // re-reading the schedule must tell a cancelled run from an absent one.
        ("POST", MIGRATIONS),
        ("GET", MIGRATIONS),
        ("GET", MIGRATION_BY_ID),
        ("DELETE", MIGRATION_BY_ID),
        // D-102's read surface. The only route in the gear whose authz object
        // (`plan`) differs from its path object (a subscription).
        ("GET", MIGRATED_ORIGIN_SNAPSHOT),
        // Slice 5's entrance: the publish mount and the approval surface.
        ("POST", PLAN_PUBLISH),
        ("GET", APPROVALS),
        ("GET", APPROVAL_COUNTS),
        ("GET", APPROVAL),
        ("POST", APPROVAL_APPROVE),
        ("POST", APPROVAL_REJECT),
        ("POST", APPROVAL_WITHDRAW),
        // Slice 5's governance config: the threshold policy a D-10 unit is opened
        // over, and D-185's way back to §6's *unset ⇒ two-person rule always* — the
        // tombstone, authored through the `PUT`'s `retire` marker rather than a verb
        // of its own, so there are two operations here and not three.
        //
        // The `PUT` requires an `If-Match` (D-186) and is therefore in
        // `if_match_routes()` below. The sentence that used to stand here said it
        // carried no precondition "because a tenant's first proposal has no prior
        // version to name and a mandatory `If-Match` would make the bootstrap
        // unreachable" — false, and withdrawn: the `GET` answers 200 with
        // `effective: null`, so there is always a representation to tag.
        ("GET", APPROVAL_THRESHOLD_POLICY),
        ("PUT", APPROVAL_THRESHOLD_POLICY),
    ]
}

/// Build every mounted router over a connected-but-empty database and hand back
/// what they registered.
///
/// The number of them is deliberately not in this sentence: it said "three" while six
/// were merged, and a count beside a roster is one more thing to keep true. **The
/// sentence then said "a seventh router" while the seventh was already in the chain
/// below** — the same defect one clause over, which is why the ordinal is gone too. The
/// merge chain below is the roster, and `rest_authz.rs`'s
/// `every_mounted_router_is_merged_into_both_censuses` is what stops **the next** router
/// being written without joining it.
///
/// No migrations: nothing here sends a request, and the registration happens
/// while the router is built. What this needs a provider for is that the states
/// hold one.
/// The two `/config` policy routers, extracted so [`registered_operations`] stays
/// under the line cap rather than growing a lint allow — `rest_authz`'s own
/// overlay extraction, for the same reason and in the same shape.
///
/// They belong together on more than length: both write `pricing_policy_object`,
/// and they are the only two things that do.
fn config_routers(
    authoring: &Arc<bss_pricing::api::rest::state::AuthoringState>,
    openapi: &OpenApiRegistryImpl,
) -> axum::Router {
    bss_pricing::api::rest::tax_display_policy::router(Arc::clone(authoring), openapi)
        .merge(bss_pricing::api::rest::rounding_policy::router(
            Arc::clone(authoring),
            openapi,
        ))
        .merge(bss_pricing::api::rest::rounding_policies::router(
            Arc::clone(authoring),
            openapi,
        ))
        .merge(bss_pricing::api::rest::gl_codes::router(
            Arc::clone(authoring),
            openapi,
        ))
        .merge(bss_pricing::api::rest::catalog_skus::router(
            std::sync::Arc::new(bss_pricing::api::rest::catalog_skus::ApiState {
                catalog: std::sync::Arc::new(
                    bss_pricing::domain::ports::UnconfiguredProductCatalogClientV1,
                ),
                source: "unconfigured",
            }),
            openapi,
        ))
}

/// The registry every service in this harness holds.
///
/// One spelling rather than eight copies of a four-line `Arc::new`: the harness is
/// about which operations mount, and a service that took a *different* registry here
/// would be a difference this file is not testing.
fn unconfigured_registry() -> Arc<dyn bss_pricing::domain::ports::CatalogVersionRegistryV1> {
    Arc::new(bss_pricing_sdk::catalog_version_registry::UnconfiguredCatalogVersionRegistryV1)
}

async fn registered_operations() -> OpenApiRegistryImpl {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    let db = DBProvider::<DbError>::new(db);
    let openapi = OpenApiRegistryImpl::new();

    let frontier_state = Arc::new(bss_pricing::api::rest::frontier::ApiState {
        pin_frontier: PinFrontierRepo::new(db.clone()),
        db: db.clone(),
    });
    let history_db = db.clone();
    let audit_db = db.clone();
    let approvals = ApprovalService::new(db.clone());
    let (compensation, _) = bss_pricing::infra::repricing::run_compensation_lane(db.clone());
    let (apply_lane, _) = bss_pricing::infra::repricing::run_apply_lane(
        db.clone(),
        bss_pricing::infra::storage::repo::PolicyObjectRepo::new(
            db.clone(),
            &LimitsConfig::default(),
        ),
        unconfigured_registry(),
        compensation,
    );
    let authoring = Arc::new(AuthoringState {
        approvals: approvals.clone(),
        db: db.clone(),
        plans: PlanRepo::new(db.clone()),
        shapes: PlanShapeRepo::new(db.clone()),
        prices: PriceRepo::new(db.clone()),
        bundles: bss_pricing::infra::storage::repo::BundleRepo::new(db.clone()),
        bundle_service: bss_pricing::infra::bundle::BundleService::new(db.clone()),
        overlays: bss_pricing::infra::storage::repo::OverlayRepo::new(db.clone()),
        taxonomies: bss_pricing::infra::storage::repo::taxonomy_repo::TaxonomyRepo::new(db.clone()),
        idempotency: IdempotencyGate::new(Duration::from_hours(1)),
    });
    // The registry is the fail-closed production default and the fixture gate is
    // loaded from a path that does not exist, which closes it. Neither matters:
    // registration happens while the router is built and nothing here sends a
    // request. Wiring a working pair would be wiring a publish this test does
    // not perform.
    let governance = Arc::new(GovernanceState {
        participants: bss_pricing::infra::approval_participants::ApprovalParticipants::new(
            Arc::new(toolkit::ClientHub::new()),
        ),
        // The lane an accepted repricing run's apply would leave on. Its applier is
        // dropped with the rest of this harness: registration happens while the
        // router is built and nothing here sends a request, so nothing is enqueued.
        apply_lane: apply_lane.clone(),
        db: db.clone(),
        idempotency: bss_pricing::infra::storage::repo::IdempotencyGate::new(
            LimitsConfig::default().idempotency_key_ttl(),
        ),
        thresholds: bss_pricing::infra::threshold::ThresholdService::new(db.clone()),
        // The no-op, for the same reason as the gate above: this test builds the
        // router to census its paths and sends no request, so there is nothing
        // to report.
        metrics: Arc::new(bss_pricing::domain::ports::metrics::NoopPricingMetrics),
        plans: PlanRepo::new(db.clone()),
        prices: PriceRepo::new(db.clone()),
        approvals,
        overlays: bss_pricing::infra::storage::repo::OverlayRepo::new(db.clone()),
        overlay_publish: bss_pricing::infra::overlay_publish::OverlayPublishService::new(
            db.clone(),
            unconfigured_registry(),
        ),
        windows: WindowService::new(db.clone(), unconfigured_registry()),
        supersessions: bss_pricing::infra::supersession::SupersessionService::new(
            db.clone(),
            unconfigured_registry(),
        ),
        cutovers: bss_pricing::infra::cutover::CutoverService::new(
            db.clone(),
            &LimitsConfig::default(),
            FixtureGate::closed(),
            unconfigured_registry(),
        ),
        grandfather: bss_pricing::infra::grandfather::GrandfatherService::new(
            db.clone(),
            unconfigured_registry(),
        ),
        retirements: bss_pricing::infra::retirement::RetirementService::new(
            db.clone(),
            unconfigured_registry(),
        ),
        // Slice 11's migration plane. Requests no `CatalogVersion`, so it
        // takes no registry - only the limits its policy reader is bound to.
        migrations: bss_pricing::infra::migration::MigrationService::new(
            db.clone(),
            &LimitsConfig::default(),
        ),
        // Slice 11's synthesis half. No registry and no limits: it freezes a
        // payload nothing can look up (D-87).
        synthesis: bss_pricing::infra::synthesis::SynthesisService::new(db.clone()),
        publish: PublishService::new(
            db.clone(),
            &LimitsConfig::default(),
            FixtureGate::load(std::path::Path::new("/nonexistent/registry.toml")),
            unconfigured_registry(),
        ),
    });

    // Task 6's membership state, `governance`'s own reason for a fresh
    // fail-closed registry per field: registration happens while the router is
    // built and this test sends no request.
    let membership_state = Arc::new(bss_pricing::api::rest::customer_groups::MembershipState {
        db,
        idempotency: IdempotencyGate::new(Duration::from_hours(1)),
        registry: unconfigured_registry(),
    });

    drop(
        bss_pricing::api::rest::frontier::router(frontier_state, &openapi)
            // Slice 12's history read, mounted here for the same reason every
            // other router is: this drop-built tree is what proves the OpenAPI
            // registrations do not collide, and a router absent from it is a
            // router whose operation id nothing checks.
            .merge(bss_pricing::api::rest::history::router(
                Arc::new(bss_pricing::api::rest::history::ApiState {
                    history: bss_pricing::infra::history::HistoryExporter::new(history_db),
                }),
                &openapi,
            ))
            // Slice 5's Auditor read, mounted here for the history read's reason.
            .merge(bss_pricing::api::rest::audit::router(
                Arc::new(bss_pricing::api::rest::audit::ApiState {
                    audit: bss_pricing::infra::audit_read::AuditReader::new(audit_db),
                }),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::bulk_imports::router(
                Arc::new(bss_pricing::api::rest::bulk_imports::ApiState {
                    authoring: Arc::clone(&authoring),
                }),
                &openapi,
            ))
            // Slice 12's mass repricing, mounted here for every other router's
            // reason: this drop-built tree is what proves the OpenAPI
            // registrations do not collide.
            .merge(bss_pricing::api::rest::repricing_runs::router(
                Arc::new(bss_pricing::api::rest::repricing_runs::ApiState {
                    authoring: Arc::clone(&authoring),
                    // `governance`'s own lane, for `governance`'s own reason.
                    apply_lane,
                }),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::plans::router(
                Arc::clone(&authoring),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::prices::router(
                Arc::clone(&authoring),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::overlays::router(
                Arc::clone(&authoring),
                &openapi,
            ))
            // The submit route publishes (D-234), so it is mounted on the
            // governance state apart from its authoring siblings.
            .merge(bss_pricing::api::rest::overlays::governance_router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::taxonomies::router(
                Arc::clone(&authoring),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::customer_groups::router(
                Arc::clone(&authoring),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::customer_groups::governance_router(
                Arc::clone(&membership_state),
                &openapi,
            ))
            .merge(config_routers(&authoring, &openapi))
            .merge(bss_pricing::api::rest::preview::router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::bundles::router(
                Arc::clone(&authoring),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::windows::router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::supersessions::router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::cutovers::router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::retirement::router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::migrations::router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::migrated_origin_snapshots::router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::approvals::router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::publish::router(
                Arc::clone(&governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::threshold_policy::router(
                governance, &openapi,
            )),
    );
    openapi
}

#[tokio::test]
async fn the_registered_route_set_is_exactly_the_declared_paths() {
    // Six groups built repositories, guards, a validator, a commit and a
    // projector, and Slice 5 gave the commit an entrance; this is the whole of
    // what an operator can reach. A route added later without a line in
    // `declared_paths` fails here rather than shipping ungated and
    // undocumented.
    let openapi = registered_operations().await;

    let mut found: Vec<String> = openapi
        .operation_specs
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    found.sort();
    let mut expected: Vec<String> = declared_paths()
        .into_iter()
        .map(|(method, path)| format!("{method}:{path}"))
        .collect();
    expected.sort();

    assert_eq!(found, expected);
}

#[tokio::test]
async fn every_registered_operation_carries_a_tag_and_a_summary() {
    // DE0205 in a test as well as in a lint: an operation without them is one an
    // operator meets in a generated client with no name and no group.
    let openapi = registered_operations().await;

    for entry in &openapi.operation_specs {
        let spec = entry.value();
        assert!(!spec.tags.is_empty(), "{} carries no tag", entry.key());
        assert!(
            spec.summary.as_ref().is_some_and(|s| !s.trim().is_empty()),
            "{} carries no summary",
            entry.key()
        );
    }
}

#[tokio::test]
async fn no_operation_declares_a_422() {
    // §3.3 states it once for the whole design set: the platform has no 422
    // category, every architectural 422 reaches the wire as a 400 carrying its
    // code, and an endpoint MUST NOT declare a response no path can produce.
    let openapi = registered_operations().await;

    for entry in &openapi.operation_specs {
        for response in &entry.value().responses {
            assert_ne!(
                response.status,
                422,
                "{} declares a 422; no path in this gear can produce one",
                entry.key()
            );
        }
    }
}

#[tokio::test]
async fn every_route_with_a_path_parameter_declares_a_400() {
    // The derivation, and not a seventh hand-maintained roster.
    //
    // A `{…}` segment is refused before the handler by axum's own `Path<T>`
    // rejection when `T` parses — 400 with **no problem document at all**, the
    // shape `rest_windows`'s `a_malformed_plan_id_never_reaches_the_handler` pins
    // — and refused *by* the handler when `T` is a `String` this gear validates
    // (`required_group`, `parse_class`, `ScopeValue::new`). Both are 400s, so
    // every parameterized route in this gear can produce one and every one must
    // say so.
    //
    // Seven did not when this was written, and one artifact in this repository
    // asserted the 400 that a second artifact in this repository denied the route
    // could emit — both green, because nothing compared them. Of the seven by-id
    // `GET`s, three declared it and four did not, with nothing distinguishing the
    // two sets.
    let openapi = registered_operations().await;

    let mut undeclared: Vec<String> = Vec::new();
    for entry in &openapi.operation_specs {
        let key = entry.key();
        // `METHOD:/path` — the parameter lives in the path half.
        if !key.contains('{') {
            continue;
        }
        if !entry.value().responses.iter().any(|r| r.status == 400) {
            undeclared.push(key.clone());
        }
    }
    undeclared.sort();
    assert!(
        undeclared.is_empty(),
        "these routes bind a path parameter and declare no 400, which a malformed segment \
         produces before their handler ever runs: {undeclared:?}"
    );
}

#[tokio::test]
async fn every_path_template_segment_is_a_declared_path_parameter() {
    // The axis the four censuses beside it did not cover (review T-1, 2026-08-19).
    //
    // `every_route_with_a_path_parameter_declares_a_400` above asks whether a
    // parameterized route declares a **400**; nothing asked whether it declares
    // the **parameter**. Nothing in the toolkit derives one either:
    // `OperationBuilder::path_param` is the only thing that pushes a
    // `ParamLocation::Path` spec, `openapi_registry` renders exactly what the spec
    // holds, and `axum_to_openapi_path` rewrites the template without consulting
    // the parameter list. So a `{…}` segment with no declaration emits a document
    // that is **structurally invalid** — OpenAPI 3.x requires every path template
    // expression to have a corresponding `in: path` parameter — and a generated
    // client either cannot fill the segment or requests the literal `%7B…%7D`.
    // Over the wire the route works, so no runtime case can see it;
    // `preview_plan_price` carried it from the day it was registered.
    //
    // **Set equality in both directions**, for the reason
    // `every_query_reading_route_declares_the_parameters_it_reads` gives for its
    // own: a declared path parameter the template does not carry is the same
    // defect from the other side — it documents a segment no caller can place.
    let openapi = registered_operations().await;

    let mut wrong: Vec<String> = Vec::new();
    for entry in &openapi.operation_specs {
        let key = entry.key();
        let Some(path) = key.split_once(':').map(|(_, path)| path) else {
            continue;
        };
        let mut templated: Vec<String> = path
            .split('/')
            .filter_map(|segment| {
                segment
                    .strip_prefix('{')
                    .and_then(|inner| inner.strip_suffix('}'))
                    .map(ToOwned::to_owned)
            })
            .collect();
        let mut declared: Vec<String> = entry
            .value()
            .params
            .iter()
            .filter(|param| matches!(param.location, ParamLocation::Path))
            .map(|param| param.name.clone())
            .collect();
        templated.sort();
        declared.sort();
        if templated != declared {
            wrong.push(format!(
                "{key}: template {templated:?}, declared {declared:?}"
            ));
        }
    }
    wrong.sort();
    assert!(
        wrong.is_empty(),
        "every `{{…}}` segment must be a declared path parameter and every declared path \
         parameter must be a segment: {wrong:?}"
    );
}

#[test]
fn no_handler_takes_axums_json_extractor() {
    // The derivation `no_operation_declares_a_422` cannot perform, and the reason
    // this scan exists rather than a fourth prose statement.
    //
    // That test reads the *declarations*; an extractor emits its status without
    // one. `axum::extract::Json`'s rejection for a body that parses as JSON but
    // not as the target type is a **422** — the status §3.3 forbids by name — and
    // for a missing `Content-Type` a **415**, and both answer plain text outside
    // the canonical `Problem` envelope. Worse, an extractor runs during dispatch,
    // *before* the handler body, so the route answers on the shape of the body
    // before `require_authenticated` has run at all: an anonymous caller is
    // fingerprinted against the request schema where every other route answers
    // 401.
    //
    // The gear stated the rule in three places in prose —
    // `preconditions::parse_body`, `prices.rs`'s router note, `approvals.rs` — and
    // one of thirty body-bearing handlers took the extractor anyway
    // (`put_threshold_policy`, until 2026-08-17). Three sentences where one scan
    // was owed.
    //
    // The needle is the **parameter** form `Json(x): Json<T>` and not `Json<` on
    // its own: `Json` as a *response* type is correct and is used by most of the
    // 67 routes, so a scan keyed on the type name would have to be weakened to
    // pass, which is the shape that makes a guard read as coverage while binding
    // nothing.
    let mut offenders: Vec<String> = Vec::new();
    for path in rest_sources() {
        if scannable(&path).contains(": Json<") {
            offenders.push(path.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "these sources bind axum's `Json` extractor in a parameter position: {offenders:?}. Take \
         `body: Bytes` and call `preconditions::parse_body` after the gate, as the other \
         body-bearing handlers do"
    );
}

/// A config provider that knows about no gear at all.
struct NoConfig;

impl toolkit::config::ConfigProvider for NoConfig {
    fn get_gear_config(&self, _gear: &str) -> Option<&serde_json::Value> {
        None
    }
}

#[tokio::test]
async fn an_unconfigured_gear_reserves_its_prefix_and_answers_404_under_it() {
    // A gear compiled in but absent from `gears:` has no runtime. It must still
    // claim `/bss-pricing/v1` - so another gear cannot take the namespace - and
    // a request under it must be a clean 404 rather than a 500 from a handler
    // reaching for state that was never built.
    let gear = BssPricingGear::default();
    let openapi = OpenApiRegistryImpl::new();
    let ctx = GearCtx::new(
        "bss-pricing",
        uuid::Uuid::new_v4(),
        Arc::new(NoConfig),
        Arc::new(toolkit::client_hub::ClientHub::new()),
        tokio_util::sync::CancellationToken::new(),
    );

    let router = gear
        .register_rest(&ctx, Router::new(), &openapi)
        .expect("an unconfigured gear still registers a router");

    assert_eq!(
        openapi.operation_specs.len(),
        0,
        "an unconfigured gear registers no operation at all"
    );

    let response = tower::ServiceExt::oneshot(
        router,
        axum::http::Request::builder()
            .method("GET")
            .uri("/bss-pricing/v1/plans/00000000-0000-0000-0000-000000000000")
            .body(axum::body::Body::empty())
            .expect("build request"),
    )
    .await
    .expect("the router answers");

    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
}

/// Every mutating route that declares a precondition header, and which one.
///
/// Transcribed rather than derived from the router, for `declared_paths`' reason:
/// a declaration dropped from a registration is invisible if the expectation is
/// read off the same registration. The count is deliberately not in this
/// sentence — the roster below is the roster, and a number beside it is one more
/// thing to keep true (the doc said "six" against a list of seven for a whole
/// phase).
///
/// **The three approval decisions are deliberately not here.** §5's idempotency
/// cell for them reads *per decision*, and the decision is at-most-once by
/// construction: `approval_repo`'s compare-and-swap carries `state =
/// 'submitted'` in its own predicate, so a retry is refused
/// `APPROVAL_NOT_PENDING` whether or not a header was sent. An approval record
/// carries no version column for an `If-Match` to name, and declaring one would
/// tell a generated client to send a precondition the server cannot test.
///
/// **`DELETE /price-windows/{windowId}` is deliberately not here either**, and it is
/// the one absence a test enforces rather than a doc asserting: §5's Idempotency cell
/// for that surface is **empty**, so it declares neither header, and
/// `the_window_cancel_declares_no_precondition_header` is what keeps a later group
/// from adding one to be helpful.
///
/// **The roster is no longer the census, and that is the 2026-08-17 repair.** It
/// carried twelve rows against fourteen routes that read an `If-Match`, and its
/// sibling four against ten that read a key, while
/// `every_mutating_route_declares_its_precondition_header` iterated it — so a route
/// absent from it was compared against nothing, which is how two `PATCH`es came to
/// require a header they declared nowhere. What binds the population now is
/// `every_precondition_reading_route_is_in_the_precondition_census`, which reads the
/// call sites out of `src/api/rest/**` and refuses a row this list is missing. The
/// list survives for what a scan cannot say: *which* header, and why.
fn if_match_routes() -> Vec<(&'static str, &'static str)> {
    use bss_pricing::api::rest::bulk_imports::{BULK_IMPORT_ABORT, BULK_IMPORTS};
    use bss_pricing::api::rest::bundles::{BUNDLE_BY_ID, BUNDLES};
    use bss_pricing::api::rest::customer_groups::{
        CUSTOMER_GROUP_MEMBER, CUSTOMER_GROUP_MEMBER_MOVE, CUSTOMER_GROUP_MEMBERS,
        CUSTOMER_GROUP_MEMBERS_MOVE, CUSTOMER_GROUP_TAXONOMY,
    };
    use bss_pricing::api::rest::cutovers::PRICE_GRANDFATHER_UNTIL;
    use bss_pricing::api::rest::gl_codes::GL_CODES;
    use bss_pricing::api::rest::overlays::{PRICE_OVERLAY_BY_ID, PRICE_OVERLAYS};
    use bss_pricing::api::rest::plans::{PLAN, PLAN_ABANDON, PLAN_CLONE, PLANS};
    use bss_pricing::api::rest::prices::{PLAN_PRICE, PLAN_PRICES};
    use bss_pricing::api::rest::publish::PLAN_PUBLISH;
    use bss_pricing::api::rest::rounding_policies::ROUNDING_POLICIES;
    use bss_pricing::api::rest::rounding_policy::ROUNDING_POLICY;
    use bss_pricing::api::rest::tax_display_policy::TAX_DISPLAY_POLICY;
    use bss_pricing::api::rest::taxonomies::TAXONOMY_VALUE;
    use bss_pricing::api::rest::threshold_policy::APPROVAL_THRESHOLD_POLICY;
    use bss_pricing::api::rest::windows::{PRICE_WINDOW, PRICE_WINDOWS};
    vec![
        ("PATCH", PLAN),
        ("POST", PLAN_ABANDON),
        ("PATCH", PLAN_PRICE),
        ("DELETE", PLAN_PRICE),
        // The publish mount: its tag names the revision it freezes **and** that
        // revision's version, which is what the commit's compare-and-swap
        // submits.
        ("POST", PLAN_PUBLISH),
        // Slice 7's window `PATCH`: §5 gives it an **ETag**, and on a window route the
        // tag is the window row's own version (D-141's rule for a price row, applied
        // to the surface that addresses one window by id).
        ("PATCH", PRICE_WINDOW),
        // Slice 7's horizon door: §5 gives it an **ETag** too, and on a price route
        // that is the row's own version (D-141). It is the one entry here whose tag
        // is **frozen** — a published row's version never moves, so this
        // precondition refuses a caller who addressed a version the row never had
        // and cannot refuse one whose *horizon* is stale. What refuses that is the
        // update's own predicate, which carries the horizon this transaction read;
        // see `price_repo::tighten_grandfather_until`. The declaration is still
        // owed: §5 names the cell, and a generated client that does not send the
        // header cannot write at all.
        ("PATCH", PRICE_GRANDFATHER_UNTIL),
        // Slice 5's policy `PUT`: §5's cell is *`ETag` + approval unit* and D-186
        // implements the first half. It is the one entry here whose tag is **not** a
        // row version — the store is append-only and has no version column — so the
        // tag is a digest over the representation the `GET` serves. The declaration
        // matters as much as on the others and for a sharper reason: this resource's
        // `GET` is the only place a tag can be obtained, so a client that does not
        // know to send one cannot write at all.
        ("PUT", APPROVAL_THRESHOLD_POLICY),
        // The creates assert their precondition through the idempotency gate rather
        // than through a version, and are listed under `idempotency_key_routes` too —
        // the window schedule among them, which is §5's own Idempotency cell for it.
        ("POST", PLANS),
        ("POST", PLAN_PRICES),
        ("POST", PRICE_WINDOWS),
        // The clone is a create too, and the one whose classification is worth
        // stating: it addresses an **existing** plan in its path, so it reads
        // like a route that would hold that plan's version. It does not — it
        // writes nothing to the source, and there is no version of a plan that
        // does not exist yet to assert about the target (D-275).
        ("POST", PLAN_CLONE),
        // The six the derived census brought in, and the pair worth naming: the two
        // `PATCH`es required the header and declared **nothing**, so a generated
        // client could not call either. The other four already declared it and were
        // simply unguarded — a distinction worth keeping, because it is the reason
        // "the roster is a subset" is a finding on its own rather than only when a
        // live defect happens to sit inside the gap.
        ("PATCH", BUNDLE_BY_ID),
        ("PATCH", PRICE_OVERLAY_BY_ID),
        // The membership adjust: `preconditions::if_match` over the membership row's
        // own version.
        ("PATCH", CUSTOMER_GROUP_MEMBER),
        // The five whole-document config `PUT`s, each asserting a `PolicyTag` over
        // the representation its own `GET` serves rather than a row version.
        ("PUT", CUSTOMER_GROUP_TAXONOMY),
        // D-353: the per-value `PATCH` asserts the **value's own** tag (the set's
        // whole-set `PUT`, which asserted the set tag, is removed).
        ("PATCH", TAXONOMY_VALUE),
        ("PUT", TAX_DISPLAY_POLICY),
        ("PUT", ROUNDING_POLICY),
        ("PUT", ROUNDING_POLICIES),
        ("PUT", GL_CODES),
        // The creates the derived census brought in, listed here for the same reason
        // the four above them are: they assert through the idempotency gate.
        ("POST", BUNDLES),
        ("POST", PRICE_OVERLAYS),
        ("POST", BULK_IMPORTS),
        ("POST", CUSTOMER_GROUP_MEMBERS),
        ("POST", CUSTOMER_GROUP_MEMBER_MOVE),
        ("POST", CUSTOMER_GROUP_MEMBERS_MOVE),
        // The abort is the one row here that reads a key and binds nothing —
        // deliberately, as its refusal rather than as a value. It is in the roster
        // because it *declares* the header and a client must send one; it is out of
        // `every_route_that_binds_an_idempotency_key_declares_a_409` because it has
        // no dedup gate for a replay to conflict with.
        ("POST", BULK_IMPORT_ABORT),
    ]
}

/// The routes that require an `Idempotency-Key` (D-141/D-142, and §5's Idempotency
/// column for the window schedule).
///
/// Every row is also in [`if_match_routes`], because a create asserts its
/// precondition through the gate rather than through a version; the two lists differ
/// in what `every_mutating_route_declares_its_precondition_header` expects to find
/// declared.
fn idempotency_key_routes() -> Vec<(&'static str, &'static str)> {
    use bss_pricing::api::rest::bulk_imports::{BULK_IMPORT_ABORT, BULK_IMPORTS};
    use bss_pricing::api::rest::bundles::BUNDLES;
    use bss_pricing::api::rest::customer_groups::{
        CUSTOMER_GROUP_MEMBER_MOVE, CUSTOMER_GROUP_MEMBERS, CUSTOMER_GROUP_MEMBERS_MOVE,
    };
    use bss_pricing::api::rest::overlays::PRICE_OVERLAYS;
    use bss_pricing::api::rest::plans::{PLAN_CLONE, PLANS};
    use bss_pricing::api::rest::prices::PLAN_PRICES;
    use bss_pricing::api::rest::windows::PRICE_WINDOWS;
    vec![
        ("POST", PLANS),
        ("POST", PLAN_PRICES),
        ("POST", PRICE_WINDOWS),
        ("POST", PLAN_CLONE),
        ("POST", BUNDLES),
        ("POST", PRICE_OVERLAYS),
        ("POST", BULK_IMPORTS),
        ("POST", BULK_IMPORT_ABORT),
        ("POST", CUSTOMER_GROUP_MEMBERS),
        ("POST", CUSTOMER_GROUP_MEMBER_MOVE),
        ("POST", CUSTOMER_GROUP_MEMBERS_MOVE),
    ]
}

/// The header parameters one registered operation declares.
fn declared_headers(openapi: &OpenApiRegistryImpl, method: &str, path: &str) -> Vec<String> {
    let key = format!("{method}:{path}");
    let entry = openapi
        .operation_specs
        .get(&key)
        .unwrap_or_else(|| panic!("{key} is not a registered operation"));
    entry
        .value()
        .params
        .iter()
        .filter(|param| matches!(param.location, ParamLocation::Header))
        .map(|param| param.name.to_ascii_lowercase())
        .collect()
}

/// The query parameters one registered operation declares, sorted.
fn declared_query_params(openapi: &OpenApiRegistryImpl, method: &str, path: &str) -> Vec<String> {
    let key = format!("{method}:{path}");
    let entry = openapi
        .operation_specs
        .get(&key)
        .unwrap_or_else(|| panic!("{key} is not a registered operation"));
    let mut names: Vec<String> = entry
        .value()
        .params
        .iter()
        .filter(|param| matches!(param.location, ParamLocation::Query))
        .map(|param| param.name.clone())
        .collect();
    names.sort();
    names
}

/// Whether one registered operation declares a request body at all.
///
/// The third axis of the same guard [`declared_headers`] and [`declared_query_params`]
/// serve: those bind a header and a query parameter the handler reads to the document
/// that promises them, and nothing bound the **body**. Six mutating routes parsed a
/// required one and declared none, so a generated client sent no body and every call
/// answered `400` "the request body is empty".
fn declares_request_body(openapi: &OpenApiRegistryImpl, method: &str, path: &str) -> bool {
    let key = format!("{method}:{path}");
    let entry = openapi
        .operation_specs
        .get(&key)
        .unwrap_or_else(|| panic!("{key} is not a registered operation"));
    entry.value().request_body.is_some()
}

/// `(method, path, extractor type, the query parameters the handler reads)`.
///
/// Named because the tuple is read by two tests that mean different things by it:
/// the third element is what binds the roster to the source scan, and the fourth is
/// what binds it to the emitted document.
type QueryReadingRoute = (&'static str, &'static str, &'static str, Vec<&'static str>);

/// Every route that takes a `Query<T>` extractor, the extractor it takes, and the
/// query parameters its handler therefore reads.
///
/// The third column is the **document's** owed set and the second is what binds it
/// to the handler: [`every_query_reading_route_is_in_the_parameter_census`] scans
/// the REST sources for `Query<…>` and fails until the extractor it finds has a row
/// here, so a new query-reading route cannot be mounted without declaring what it
/// reads. Without that scan the roster is a hand-enumeration of the same shape as
/// the defect it is checking — the F-12 class — and its own predecessor showed why:
/// it listed the three routes a fix wave had just visited and was silent about the
/// nine others.
fn query_reading_routes() -> Vec<QueryReadingRoute> {
    use bss_pricing::api::rest::approvals::APPROVALS;
    use bss_pricing::api::rest::audit::AUDIT;
    use bss_pricing::api::rest::bundles::{BUNDLE_BY_ID, BUNDLES};
    use bss_pricing::api::rest::customer_groups::CUSTOMER_GROUP_MEMBERS;
    use bss_pricing::api::rest::history::{HISTORY, HISTORY_EXPORT};
    use bss_pricing::api::rest::migrations::MIGRATIONS;
    use bss_pricing::api::rest::overlays::PRICE_OVERLAYS;
    use bss_pricing::api::rest::plans::PLANS;
    use bss_pricing::api::rest::preview::PLAN_PREVIEW;
    use bss_pricing::api::rest::prices::PLAN_PRICES;
    use bss_pricing::api::rest::windows::{PLAN_SELLABILITY, PRICE_WINDOWS_LIST};
    vec![
        // D-125's cursor walks. Collection GETs take `Query<HashMap>` plus the
        // OData extractor; named filter keys are retired. `limit` and `cursor`
        // stay on the wire (AM tenants). The unique-set census collapses every
        // HashMap row to one extractor name.
        (
            "GET",
            PRICE_OVERLAYS,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        (
            "GET",
            HISTORY,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        // The export is **not** an OData list: same `{limit, cursor}` spelling,
        // HistoryQuery, no `$filter` / `$orderby`.
        (
            "POST",
            HISTORY_EXPORT,
            "HistoryQuery",
            vec!["cursor", "limit"],
        ),
        (
            "GET",
            AUDIT,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        (
            "GET",
            PLANS,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        (
            "GET",
            PLAN_PRICES,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        (
            "GET",
            APPROVALS,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        (
            "GET",
            BUNDLES,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        (
            "GET",
            PRICE_WINDOWS_LIST,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        (
            "GET",
            MIGRATIONS,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        (
            "GET",
            CUSTOMER_GROUP_MEMBERS,
            "HashMap",
            vec!["$filter", "$orderby", "cursor", "limit"],
        ),
        // The reads whose query is not a page. `plan_revision` was the last
        // undeclared parameter in the gear: the description *narrated* it ("absent,
        // it is the plan's open draft"), and a generated client could not send it,
        // so the composition D-310 made readable was readable at one revision only.
        (
            "GET",
            BUNDLE_BY_ID,
            "ReadBundleQuery",
            vec!["plan_revision"],
        ),
        (
            "GET",
            PLAN_SELLABILITY,
            "SellabilityQuery",
            vec!["at", "currency", "region"],
        ),
        (
            "GET",
            PLAN_PREVIEW,
            "PreviewQuery",
            vec!["currency", "region"],
        ),
    ]
}

/// **Every query-reading route declares every query parameter its handler reads.**
///
/// Asserted against the emitted document rather than against the handler, because
/// the document is the only half a generated client sees: `GET /price-overlays`
/// took `Query<HashMap>` plus `$filter` / `$orderby` — `limit`, `cursor` — and
/// declared **none** of the three, so the endpoint D-125's pagination work had just
/// paginated could not be paged by any generated client, and the narrowing filter
/// could not be sent at all (Z13-10).
///
/// **Set equality, not containment.** A declaration a handler does not read is the
/// same defect from the other side: it tells a client to send something the server
/// ignores, which is what `a_read_route_declares_no_precondition_header` says one
/// plane over about headers.
///
/// **The roster is now every `Query<T>` route and not the three a fix wave had
/// visited.** The predecessor of this test covered `/price-overlays`, `/history`
/// and `/audit`, and its own doc reported "six more collection reads take a page
/// query and declare nothing" as a remainder. That remainder was **false**: all six
/// declare `limit`, `cursor` and their filter through `.query_param_typed(…)` /
/// `.query_param(…)`, which is a second spelling of `.param(ParamSpec)` — the same
/// grep-shape mistake, in mirror, that the entry made about `/history`. What the
/// wider census did find is one genuine survivor, `GET /bundles/{bundleId}`'s
/// `plan_revision`, and that is what the accompanying commit declares.
/// The members of every `Query<T>` extractor type, by type name.
///
/// The parser [`no_query_struct_lets_the_extractor_answer`] runs over the same
/// declarations, lifted out so that the census above can compare a route's
/// **declared** parameters against the handler's **actual** read set rather than
/// against a second declaration (review T-2, 2026-08-19). Unresolved types are
/// returned as such rather than skipped, for that test's stated reason: a census
/// that cannot find its subject has not cleared it.
fn query_extractor_fields() -> (BTreeMap<String, Vec<String>>, Vec<String>) {
    let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unresolved: Vec<String> = Vec::new();
    for source in rest_sources() {
        let text = scannable(&source);
        for after in text.split("Query<").skip(1) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name == "HashMap" {
                continue;
            }
            let Some(body) = text
                .split_once(&format!("struct {name} {{"))
                .and_then(|(_, rest)| rest.split_once('}'))
                .map(|(body, _)| body)
            else {
                unresolved.push(format!("{name} (extracted in {})", source.display()));
                continue;
            };
            let mut members: Vec<String> = Vec::new();
            for member in body.split(',') {
                let Some((field, _ty)) = member.split_once(": ") else {
                    continue;
                };
                let Some(field) = field.split_whitespace().next_back() else {
                    continue;
                };
                if !field.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                members.push(field.to_owned());
            }
            members.sort();
            fields.insert(name, members);
        }
    }
    unresolved.sort();
    unresolved.dedup();
    (fields, unresolved)
}

/// Handlers that read the raw query map without taking the `OData` extractor.
///
/// A `Query<HashMap<String, String>>` row in the roster is an `OData` collection
/// GET: the map is there only so the extra-key guard can see retired named
/// filters, and `$filter` / `$orderby` are read by the `OData` extractor beside
/// it. The roster's fourth column claims those parameters, and `OpenAPI` declares
/// them — but both are declarations. A handler that dropped `OData(_)` would
/// keep them declared and silently ignore a `$filter`, answering 200 with an
/// unfiltered page, which is `Query<ListOverlaysQuery>`'s defect one seam over.
///
/// Scanned per signature rather than per file: two handlers in one module would
/// satisfy a file-level count while leaving the list one unchecked.
fn raw_query_handlers_without_odata() -> Vec<String> {
    let mut offenders = Vec::new();
    for source in rest_sources() {
        let text = scannable(&source);
        for after in text.split("async fn ").skip(1) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let Some((signature, _)) = after.split_once(") -> ") else {
                continue;
            };
            if signature.contains("Query<HashMap<String, String>>") && !signature.contains("OData(")
            {
                offenders.push(format!("{name} (in {})", source.display()));
            }
        }
    }
    offenders
}

#[tokio::test]
async fn every_query_reading_route_declares_the_parameters_it_reads() {
    let openapi = registered_operations().await;
    // **The handler's own members are the operand** (review T-2, 2026-08-19).
    // This compared `declared_query_params` with the roster's fourth column, and
    // both sides of that comparison are *declarations*: the fields of `T` — what
    // the handler can actually read — were never an operand of any assertion, so
    // adding an `Option<String>` member and reading it while declaring nothing
    // stayed green through every census in this file. That is Z13-10, the defect
    // this test exists for, reachable through the one seam its repair left open.
    let (fields, unresolved) = query_extractor_fields();
    assert!(
        unresolved.is_empty(),
        "this scan could not find the declaration of these `Query<…>` types, so it would have \
         cleared their routes without reading a member: {unresolved:?}"
    );
    let map_only = raw_query_handlers_without_odata();
    assert!(
        map_only.is_empty(),
        "these handlers read the raw query map but take no `OData` extractor, so the `$filter` \
         their route declares is ignored rather than applied: {map_only:?}"
    );

    for (method, path, extractor, expected) in query_reading_routes() {
        let mut expected = expected;
        expected.sort_unstable();
        if extractor == "HashMap" {
            // A raw-map row has no members to scan, so its read side is checked
            // by `raw_query_handlers_without_odata` below rather than skipped -
            // leaving it unchecked cleared all ten OData collection GETs.
            assert!(
                expected.contains(&"$filter"),
                "{method} {path} is rostered with the raw query map but claims no `$filter`; the \
                 map exists for the extra-key guard on an OData list"
            );
        } else {
            let read = fields.get(extractor).unwrap_or_else(|| {
                panic!("{extractor} is extracted by no source under src/api/rest")
            });
            assert_eq!(
                read, &expected,
                "the roster says {method} {path} reads {expected:?}, and {extractor} has members \
                 {read:?}"
            );
        }
        assert_eq!(
            declared_query_params(&openapi, method, path),
            expected,
            "{method} {path} declares a query parameter set its handler does not read, or reads \
             one it does not declare"
        );
    }
}

#[test]
fn no_source_reads_token_scopes() {
    // A guard on a **fixture**, not on the gear, and it is the only shape that
    // works: `rest_support::ctx_for_principal` sets `token_scopes: ["*"]` on every
    // client it builds, `denied()` included. Nothing under `src/` reads the field,
    // so the wildcard hides nothing today — and the day a scope check lands it
    // would hide all of it, every refusal built on it passing against a fixture
    // that grants everything.
    //
    // The gear is what is scanned because the fixture cannot see its own future:
    // the wildcard becomes a defect at the moment a reader appears, and this is
    // where that moment is noticed.
    //
    // **The whole of `src`, not the REST layer.** A scope check is as effective in
    // `infra`, `domain`, `authz.rs` or `module.rs` as in a handler, and the
    // fixture's wildcard reaches every one of them through the same
    // `SecurityContext`; a scan stopping at `src/api/rest` would let a reader
    // outside it grant everything while this case read as the guard against
    // exactly that.
    let sources = crate_sources();
    assert!(
        sources.len() > rest_sources().len(),
        "the gear-wide walk found {} files against the REST layer's {} -- it is not reaching \
         past `src/api/rest`",
        sources.len(),
        rest_sources().len()
    );
    let mut readers: Vec<String> = Vec::new();
    for source in sources {
        if scannable(&source).contains("token_scopes") {
            readers.push(source.display().to_string());
        }
    }
    assert!(
        readers.is_empty(),
        "these sources read `token_scopes` while every harness client is built with the wildcard \
         `[\"*\"]`, so a refusal keyed on a scope cannot be tested: {readers:?}. Narrow \
         `rest_support::ctx_for_principal` before relying on the field"
    );
}

#[test]
fn no_query_struct_lets_the_extractor_answer() {
    // The other half of `no_handler_takes_axums_json_extractor`, on the parameter
    // axis, and the lesson `windows.rs` recorded once and applied to one struct of
    // twelve.
    //
    // A `Query<T>` member that is not `Option<String>` is parsed by axum, and its
    // rejection is a bare 400 with **no problem document at all** — against a
    // registration whose declared 400 has `Problem` as its schema. `?limit=abc` on
    // any of the nine paginated reads answered exactly that. The remedy is the one
    // `SellabilityQuery` already used: optional strings at the type, required and
    // parsed in the handler, so the refusal names the parameter through the
    // canonical ladder.
    //
    // The whole struct is scanned rather than the `limit` member, because the two
    // uuid members and the `plan_revision` had the same defect and no roster of
    // member names would have found them.
    //
    // **It matched `pub struct {name} {` and skipped what it could not find,
    // which made it fail open** (2026-08-18 review, Z6-1). `preview.rs`'s
    // `PreviewQuery` is declared without `pub` — it is `Query`-extracted in the
    // same module and needs no wider visibility — so the one query struct the
    // scan could not resolve was the one it silently exempted, and the member
    // filter below exempted every non-`pub` member of every struct on top of
    // that. Both are closed here, and the miss is now a **failure** rather than
    // a `continue`: a census that cannot find its subject has not cleared it.
    // That is the same property `the_registration_scan_reads_the_whole_route_set`
    // holds over the route scan, applied to this one.
    let mut offenders: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    for source in rest_sources() {
        let text = scannable(&source);
        for after in text.split("Query<").skip(1) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name == "HashMap" {
                continue;
            }
            // No `pub` in the needle: it matches `pub struct X {` and `struct X {`
            // alike, and the trailing ` {` is what stops `struct Foo {` matching
            // `struct FooBar {`.
            let Some(body) = text
                .split_once(&format!("struct {name} {{"))
                .and_then(|(_, rest)| rest.split_once('}'))
                .map(|(body, _)| body)
            else {
                unresolved.push(format!("{name} (extracted in {})", source.display()));
                continue;
            };
            for member in body.split(',') {
                let Some((field, ty)) = member.split_once(": ") else {
                    continue;
                };
                // The **last** token before the colon, so a `pub` or a `#[serde…]`
                // ahead of the name is skipped without the name having to carry
                // one. Taking `pub` as required is what let a private member
                // through.
                let Some(field) = field.split_whitespace().next_back() else {
                    continue;
                };
                if !field.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                if ty.trim() != "Option<String>" {
                    offenders.push(format!("{name}.{field} is {}", ty.trim()));
                }
            }
        }
    }
    unresolved.sort();
    unresolved.dedup();
    assert!(
        unresolved.is_empty(),
        "this scan could not find the declaration of these `Query<…>` types, so it cleared them \
         without reading a single member: {unresolved:?}. Resolve them or narrow the scan \
         deliberately — an unfindable subject is not a clean one"
    );
    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "these query members are parsed by axum's extractor, which answers a bare 400 with no \
         problem document: {offenders:?}. Take them as `Option<String>` and parse in the handler"
    );
}

/// The forcing function under the roster above: a route that reads a query it does
/// not declare cannot be mounted without a decision.
///
/// The scan is over the **stripped** source of every file at or under
/// `src/api/rest`, for `Query<…>`'s extractor type, and the found set must equal
/// the roster's second column exactly. Stripped and not raw for
/// `every_mounted_router_is_merged_into_both_censuses`'s reason (Z12-8): a
/// commented-out extractor leaves its own needle in the file, so a raw-text scan
/// passes over the regression it exists to catch.
///
/// Equality in both directions, because both directions are defects: an extractor
/// with no row is a route whose parameters nothing checks, and a row with no
/// extractor is a roster entry describing a route that no longer reads a query.
#[test]
fn every_query_reading_route_is_in_the_parameter_census() {
    let mut found: Vec<String> = Vec::new();
    for path in rest_sources() {
        let text = scannable(&path);
        let mut rest = text.as_str();
        // `Query<` and not `: Query<`: the extractor is written
        // `Query(query): Query<T>`, and a formatter is free to break before the
        // colon, which `scannable` collapses but does not delete.
        while let Some(at) = rest.find("Query<") {
            rest = &rest[at + "Query<".len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                found.push(name);
            }
        }
    }
    let found: std::collections::BTreeSet<String> = found.into_iter().collect();
    let rostered: std::collections::BTreeSet<String> = query_reading_routes()
        .into_iter()
        .map(|(_, _, extractor, _)| extractor.to_owned())
        .collect();

    assert!(
        found.len() >= 3,
        "the scan found {found:?}, which is fewer query extractors than this gear has had since \
         Slice 4 - the scan is broken, not the layer"
    );
    assert_eq!(
        found, rostered,
        "a `Query<T>` extractor under src/api/rest is absent from `query_reading_routes()`, or a \
         row there names an extractor no route takes: nothing then checks that the route declares \
         what it reads"
    );
}

/// Every `.rs` file at or under `src/api/rest`, recursively — `rest_authz.rs`'s
/// `rest_sources` with its reason: a guard that a future subdirectory switches off
/// silently is a guard that reads as coverage to everyone who greps for it.
///
/// Re-typed rather than shared because a test binary is a compilation unit of its
/// own and this file has no `mod` of its own to hang it on; the two copies are four
/// lines and are pinned to the same directory by the assertion above.
fn rest_sources() -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/rest");
    let mut found = vec![root.with_extension("rs")];
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    found
}

/// Every `.rs` file at or under `src`, recursively — the whole gear.
///
/// `rest_sources` answers questions about the REST layer's own shape (what a
/// handler extracts, what a registration declares). A question about what the
/// *gear* reads is answered by the gear: a reader in `infra` or `domain` is as
/// invisible to a fixture as one in a handler, so a scan narrower than the crate
/// would report a hole it never looked into as absent.
fn crate_sources() -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    found
}

/// One line of source with every `//` comment removed, so a construct written
/// inside a comment cannot satisfy a scan.
fn scannable(path: &std::path::Path) -> String {
    std::fs::read_to_string(path)
        .expect("a readable REST source")
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every `pub const NAME: &str = "/bss-pricing/…"` declared under `src/api/rest`.
///
/// A registration names its path as a `const` about a fifth of the time, so a scan
/// that read only the string literals would silently skip those routes — the
/// failure mode that makes a derived census read as coverage while binding a
/// subset, which is the one this whole arrangement exists to close.
fn route_path_consts() -> std::collections::BTreeMap<String, String> {
    let mut found = std::collections::BTreeMap::new();
    for path in rest_sources() {
        for line in std::fs::read_to_string(&path)
            .expect("a readable REST source")
            .lines()
        {
            let line = line.trim();
            let Some(after) = line.strip_prefix("pub const ") else {
                continue;
            };
            let Some((name, rest)) = after.split_once(": &str = ") else {
                continue;
            };
            let Some(value) = rest
                .trim()
                .strip_prefix('"')
                .and_then(|v| v.split('"').next())
            else {
                continue;
            };
            if value.starts_with("/bss-pricing/") {
                found.insert(name.to_owned(), value.to_owned());
            }
        }
    }
    found
}

/// `(method, path, handler)` for every `OperationBuilder` registration, read from
/// the source rather than from the registry. The handler is spelled
/// `module::name` — the module the registration was scanned from, which is the one
/// whose `name` it resolves to.
///
/// From the *source*, because the registry knows a route's path and its declared
/// parameters and does not know which function serves it — and the handler is the
/// half every derivation below needs: what a route *does* is in its body, and what
/// it *declares* is in its registration, and the whole class of finding this closes
/// is the two disagreeing.
fn registered_handlers() -> Vec<(String, String, String)> {
    let consts = route_path_consts();
    let mut found = Vec::new();
    for source in rest_sources() {
        let module = module_of(&source);
        let text = scannable(&source);
        for block in text.split("OperationBuilder::").skip(1) {
            // The block runs to the next registration; `.register(` ends it, and
            // taking the shorter of the two keeps a `.handler(` from a following
            // block out of this one.
            let block = block.split(".register(").next().unwrap_or(block);
            let Some((method, rest)) = block.split_once('(') else {
                continue;
            };
            let Some(argument) = rest.split(')').next() else {
                continue;
            };
            let argument = argument.trim();
            let path = match argument.strip_prefix('"').and_then(|a| a.split('"').next()) {
                Some(literal) => literal.to_owned(),
                None => match consts.get(argument) {
                    Some(resolved) => resolved.clone(),
                    // Not a route registration — `OperationBuilder::get` is the
                    // only shape here, so anything unresolvable is a parse fault
                    // rather than a route, and the count assertion below catches it.
                    None => continue,
                },
            };
            let Some(handler) = block
                .split(".handler(")
                .nth(1)
                .and_then(|after| after.split(')').next())
            else {
                continue;
            };
            found.push((
                method.to_ascii_uppercase(),
                path,
                format!("{module}::{}", handler.trim()),
            ));
        }
    }
    found
}

/// The module a REST source declares: its file stem, which is the path segment
/// every registration inside it resolves its handler through.
fn module_of(source: &std::path::Path) -> String {
    source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("a REST source with a name")
        .to_owned()
}

/// Every function declared under `src/api/rest` with its own text, in scan order,
/// keyed `module::name`.
///
/// Split at each `fn ` rather than brace-matched: a body carries format strings
/// full of `{…}`, so a brace counter would mis-close on the first refusal message
/// it met. Splitting at the item boundary needs no such counting and attributes a
/// private helper sitting between two handlers to itself rather than to the
/// handler above it.
///
/// A `Vec` and not a map, because the duplicates are the point: the guard test
/// counts declarations per key, and a key that names two items is a census that
/// reads whichever the filesystem handed over first.
fn function_declarations() -> Vec<(String, String)> {
    let mut found = Vec::new();
    for source in rest_sources() {
        let module = module_of(&source);
        let text = scannable(&source);
        let mut chunks: Vec<usize> = Vec::new();
        let mut at = 0;
        while let Some(hit) = text[at..].find("fn ") {
            let start = at + hit;
            at = start + 3;
            // `fn` must stand alone: `#[cfg]`-free, and not the tail of an
            // identifier like `authz_fn`.
            if start > 0 && !text.as_bytes()[start - 1].is_ascii_whitespace() {
                continue;
            }
            chunks.push(start);
        }
        for (index, start) in chunks.iter().enumerate() {
            let end = chunks.get(index + 1).copied().unwrap_or(text.len());
            let body = &text[*start..end];
            let name: String = body["fn ".len()..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                continue;
            }
            found.push((format!("{module}::{name}"), body.to_owned()));
        }
    }
    found
}

/// Every function's own text under `src/api/rest`, keyed `module::name`.
///
/// First wins **within** a module, and only there: an `impl From` block declares a
/// `from` in a file that already has several, and none of them is a handler.
/// `the_registration_scan_reads_the_whole_route_set` asserts that no *registered*
/// handler's key is declared twice, which is the collision that would matter.
fn function_texts() -> std::collections::BTreeMap<String, String> {
    let mut found = std::collections::BTreeMap::new();
    for (key, body) in function_declarations() {
        found.entry(key).or_insert(body);
    }
    found
}

/// `(method, path)` for every registered route whose handler's body contains
/// `needle`.
fn routes_whose_handler_calls(needle: &str) -> std::collections::BTreeSet<(String, String)> {
    let bodies = function_texts();
    registered_handlers()
        .into_iter()
        .filter(|(_, _, handler)| {
            bodies
                .get(handler)
                .is_some_and(|body| body.contains(needle))
        })
        .map(|(method, path, _)| (method, path))
        .collect()
}

#[test]
fn the_registration_scan_reads_the_whole_route_set() {
    // The guard on the derivations below, and the reason it is its own test: a
    // scan that parsed nothing would make every census beneath it vacuously true,
    // which is the exact way a derived roster becomes worse than a hand-written
    // one — it reads as coverage and binds nothing.
    let handlers = registered_handlers();
    assert_eq!(
        handlers.len(),
        declared_paths().len(),
        "the source scan found {} registrations against {} declared paths; the parse is broken, \
         not the layer",
        handlers.len(),
        declared_paths().len()
    );

    let scanned: std::collections::BTreeSet<(String, String)> = handlers
        .iter()
        .map(|(method, path, _)| (method.clone(), path.clone()))
        .collect();
    let declared: std::collections::BTreeSet<(String, String)> = declared_paths()
        .into_iter()
        .map(|(method, path)| (method.to_owned(), path.to_owned()))
        .collect();
    assert_eq!(
        scanned, declared,
        "the source scan and `declared_paths()` name different route sets"
    );

    // And every handler it named is a function the body scan can find, or the
    // needle tests below quietly match nothing for that route.
    let bodies = function_texts();
    let missing: Vec<&String> = handlers
        .iter()
        .filter(|(_, _, handler)| !bodies.contains_key(handler))
        .map(|(_, _, handler)| handler)
        .collect();
    assert!(
        missing.is_empty(),
        "these handlers were registered and their bodies were not found: {missing:?}"
    );

    // And no registered handler's key names **two** declarations, or one of the
    // two routes is censused against the other one's body.
    //
    // Not hypothetical: `get_policy` and `put_policy` are each declared in both
    // `rounding_policy.rs` and `tax_display_policy.rs` and all four are
    // registered, so a body keyed by the bare name held one of each pair and two
    // of the four policy routes were read off the other module's handler —
    // first-wins by `read_dir` order, which is to say the verdict differed by
    // machine. `function_declarations` keys by module for that reason and this is
    // what holds it there: strip the qualification and all four routes name a key
    // with two declarations.
    let mut declarations: BTreeMap<String, usize> = BTreeMap::new();
    for (key, _) in function_declarations() {
        *declarations.entry(key).or_default() += 1;
    }
    let ambiguous: Vec<&String> = handlers
        .iter()
        .filter(|(_, _, handler)| declarations.get(handler).is_some_and(|count| *count > 1))
        .map(|(_, _, handler)| handler)
        .collect();
    assert!(
        ambiguous.is_empty(),
        "these registered handlers resolve to more than one declaration, so every derived census \
         reads whichever the filesystem handed over first: {ambiguous:?}"
    );
}

#[test]
fn every_precondition_reading_route_is_in_the_precondition_census() {
    // The derivation both rosters were owed, and the failure mode is filed:
    // `if_match_routes()` carried 12 rows against 14 routes that read an
    // `If-Match`, and `idempotency_key_routes()` 4 against 10 that read a key.
    // `every_mutating_route_declares_its_precondition_header` iterates the roster,
    // so a route absent from it was compared against nothing — and that test's own
    // doc records the roster going stale twice, which is the tell.
    //
    // Two of the six unrostered If-Match readers declared no header at all
    // (`PATCH /bundles/{bundleId}`, `PATCH /price-overlays/{overlayId}`), so the
    // subset was not merely untidy: a generated client could not call either.
    //
    // Modelled on `every_query_reading_route_is_in_the_parameter_census`, which is
    // the same guard one axis over.
    let if_match = routes_whose_handler_calls("preconditions::if_match");
    let idempotency = routes_whose_handler_calls("preconditions::idempotency_key");

    assert!(
        if_match.len() >= 14 && idempotency.len() >= 10,
        "the scan found {} If-Match readers and {} key readers, fewer than this gear has had \
         since Slice 7 - the scan is broken, not the layer",
        if_match.len(),
        idempotency.len()
    );

    let rostered: std::collections::BTreeSet<(String, String)> = if_match_routes()
        .into_iter()
        .chain(idempotency_key_routes())
        .map(|(method, path)| (method.to_owned(), path.to_owned()))
        .collect();

    let unrostered: Vec<&(String, String)> = if_match
        .union(&idempotency)
        .filter(|route| !rostered.contains(*route))
        .collect();
    assert!(
        unrostered.is_empty(),
        "these routes read a precondition and are in neither roster, so nothing checks that they \
         declare it: {unrostered:?}"
    );
}

#[tokio::test]
async fn every_precondition_reading_route_declares_the_header_it_reads() {
    // The half a roster cannot do at all: the roster says *a* precondition is
    // declared, this says the declared header is the one the handler reads. Both
    // are needed - `every_mutating_route_declares_its_precondition_header` accepts
    // an `Idempotency-Key` in place of an `If-Match` for the creates, which is
    // right for them and would hide a `PATCH` that declared the wrong one.
    let openapi = registered_operations().await;

    for (method, path) in routes_whose_handler_calls("preconditions::if_match") {
        let headers = declared_headers(&openapi, &method, &path);
        assert!(
            headers.iter().any(|name| name == "if-match"),
            "{method} {path} reads an If-Match and declares none: {headers:?}"
        );
    }
    for (method, path) in routes_whose_handler_calls("preconditions::idempotency_key") {
        let headers = declared_headers(&openapi, &method, &path);
        assert!(
            headers.iter().any(|name| name == "idempotency-key"),
            "{method} {path} reads an Idempotency-Key and declares none: {headers:?}"
        );
    }
}

#[tokio::test]
async fn every_body_reading_route_declares_the_body_it_parses() {
    // The axis the census did not have. `every_query_reading_route_declares_the_parameters_it_reads`
    // binds a `Query<T>` to its declared parameters and
    // `every_precondition_reading_route_declares_the_header_it_reads` binds an
    // `If-Match` read to its declared header; nothing bound `preconditions::parse_body`
    // to a declared request body, which is how six routes in two files drifted at once
    // rather than one route drifting six times.
    let openapi = registered_operations().await;
    let readers = routes_whose_handler_calls("preconditions::parse_body");

    // The floor a vacuous scan trips on: a needle that matches nothing would make
    // every assertion below pass while proving nothing.
    assert!(
        readers.len() >= 18,
        "the scan found {} body readers, fewer than this gear has mounted since Slice 7 - \
         the scan is broken, not the layer",
        readers.len()
    );

    let undeclared: Vec<&(String, String)> = readers
        .iter()
        .filter(|(method, path)| !declares_request_body(&openapi, method, path))
        .collect();
    assert!(
        undeclared.is_empty(),
        "these routes parse a required JSON body and declare none, so a generated client \
         sends no body and the call answers 400: {undeclared:?}"
    );
}

#[tokio::test]
async fn every_success_status_a_handler_returns_is_declared() {
    // The fourth axis, and the one D-2 and D-3 are: a handler may answer a success
    // status its operation never declares, and no census compared the two. The
    // retirement door's *default* arm (`dry_run` defaults to `true`) answered `200`
    // with a preview while the document declared only `202`, so the modal request -
    // the one a caller makes by omitting the flag - had an untypeable body.
    //
    // Only the 2xx statuses, and only those written literally in the handler's own
    // body: the error ladder declares its own arms through `error_400` and friends,
    // and a status reached through a helper is out of this scan's sight by design.
    let openapi = registered_operations().await;
    let bodies = function_texts();

    let success = [
        ("StatusCode::OK", 200u16),
        ("StatusCode::CREATED", 201),
        ("StatusCode::ACCEPTED", 202),
        ("StatusCode::NO_CONTENT", 204),
    ];

    let mut seen = 0usize;
    let mut undeclared: Vec<String> = Vec::new();
    for (method, path, handler) in registered_handlers() {
        let Some(body) = bodies.get(&handler) else {
            continue;
        };
        let key = format!("{method}:{path}");
        let entry = openapi
            .operation_specs
            .get(&key)
            .unwrap_or_else(|| panic!("{key} is not a registered operation"));
        let declared: Vec<u16> = entry.value().responses.iter().map(|r| r.status).collect();
        for (needle, status) in success {
            if !body.contains(needle) {
                continue;
            }
            seen += 1;
            if !declared.contains(&status) {
                undeclared.push(format!(
                    "{method} {path} answers {status} and declares {declared:?}"
                ));
            }
        }
    }

    // The floor a vacuous scan trips on.
    assert!(
        seen >= 20,
        "the scan found {seen} literal success statuses across the layer, far fewer than this \
         gear returns - the scan is broken, not the doors"
    );
    assert!(
        undeclared.is_empty(),
        "these handlers answer a success status their operation never declares, so the \
         response body is untypeable for a generated client: {undeclared:#?}"
    );
}

/// The mutating routes that assert **no** precondition, each with the reason.
///
/// A positive list, and unusually that is the right shape here: this is the set the
/// derived census must *not* find a precondition call in, so it is compared for
/// equality against the derivation rather than iterated. A route that starts
/// asserting one, or a new mutating route that asserts none, moves the set and the
/// comparison reddens either way.
///
/// **Three of these are review finding Z6-3-2 and are asymmetric with a sibling in
/// their own module**, recorded here rather than silently: `POST /bundles/{id}/publish`
/// against `POST /plans/{planId}/publish`, which asserts the revision it was composed
/// against; `POST /price-overlays/{id}/submit` against `PATCH /price-overlays/{id}`,
/// which addresses the same overlay by the same id and asserts a tag; and
/// `POST /plans/{planId}/cutovers` against `PATCH /prices/{id}/grandfather-until`,
/// two handlers in one file. Adding a required header to a live route is a contract
/// change and not this test's to make; what this list does is stop the asymmetry
/// being invisible.
fn routes_asserting_no_precondition() -> Vec<(&'static str, &'static str)> {
    use bss_pricing::api::rest::approvals::{APPROVAL_APPROVE, APPROVAL_REJECT, APPROVAL_WITHDRAW};
    use bss_pricing::api::rest::bundles::BUNDLE_PUBLISH;
    use bss_pricing::api::rest::cutovers::PLAN_CUTOVERS;
    use bss_pricing::api::rest::migrations::{MIGRATION_BY_ID, MIGRATIONS};
    use bss_pricing::api::rest::overlays::PRICE_OVERLAY_SUBMIT;
    use bss_pricing::api::rest::repricing_runs::{REPRICING_RUN_ABORT, REPRICING_RUNS};
    use bss_pricing::api::rest::retirement::PLAN_RETIRE;
    use bss_pricing::api::rest::supersessions::PLAN_SUPERSESSIONS;
    use bss_pricing::api::rest::taxonomies::TAXONOMY_VALUES;
    use bss_pricing::api::rest::windows::PRICE_WINDOW;
    vec![
        // D-353: the per-value taxonomy `POST`. The value is the resource's natural
        // key, so the create is idempotent on it without a header — a repeat with
        // the same body replays (200), other content is `409 TAXONOMY_VALUE_EXISTS`
        // — and asserting an `If-Match` would make a *create* assert the version of
        // a set it does not replace. `PLAN_SUPERSESSIONS`' reasoning, one value wide.
        ("POST", TAXONOMY_VALUES),
        // Argued and guarded: an approval carries no version column, and the
        // compare-and-swap carries `state = 'submitted'` in its own predicate, so a
        // retry is refused `APPROVAL_NOT_PENDING` whether or not a header was sent.
        ("POST", APPROVAL_APPROVE),
        ("POST", APPROVAL_REJECT),
        ("POST", APPROVAL_WITHDRAW),
        // §5's Idempotency cell is empty and
        // `the_window_cancel_declares_no_precondition_header` is the guard.
        ("DELETE", PRICE_WINDOW),
        // Its key is `run_id` **inside the body**, so it reads no
        // header and is invisible to the header census by design;
        // `the_repricing_run_declares_no_precondition_header` guards that.
        ("POST", REPRICING_RUNS),
        // The abort names its run in the path, and this plane's idempotency column is
        // that `run_id`. What makes the retry safe is the state guard plus the note
        // the sweep writes — a replayed abort is answered the run it ended — and
        // neither needs a header. Its bulk sibling *does* take a key, which is the
        // asymmetry `the_repricing_run_declares_no_precondition_header` was written
        // against: adding one here would tell every generated client to send a header
        // this server never reads.
        ("POST", REPRICING_RUN_ABORT),
        // The three asymmetries named in this function's doc.
        ("POST", BUNDLE_PUBLISH),
        ("POST", PRICE_OVERLAY_SUBMIT),
        ("POST", PLAN_CUTOVERS),
        // The four with no sibling to be asymmetric with. Each is a create or a
        // lifecycle move over a subject the request names in its path, and none has
        // a version a caller could have read.
        ("POST", PLAN_RETIRE),
        ("POST", PLAN_SUPERSESSIONS),
        ("POST", MIGRATIONS),
        ("DELETE", MIGRATION_BY_ID),
    ]
}

#[test]
fn the_mutating_routes_that_assert_nothing_are_exactly_the_stated_ones() {
    // What `preconditions.rs` has no structure to notice: it is nine free functions
    // each handler opts into by calling, so there is no verb dispatch to leave a
    // verb unhandled — and nothing that observes a mutating route calling none of
    // them.
    //
    // Derived from the same scan the two censuses use, so the negatives are as
    // bound as the positives. Before this, eight mutating routes asserted nothing
    // and only two of the eight were written down anywhere.
    let asserting: std::collections::BTreeSet<(String, String)> =
        routes_whose_handler_calls("preconditions::if_match")
            .union(&routes_whose_handler_calls(
                "preconditions::idempotency_key",
            ))
            .cloned()
            .collect();

    let mut silent: Vec<(String, String)> = registered_handlers()
        .into_iter()
        .filter(|(method, path, _)| {
            matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE")
                // The one `POST` that is a **read**: `inst-he-nostore` leaves the
                // export nothing to write, which is why `declared_paths()` lists it
                // among the reads and `rest_authz`'s census carries it as
                // `mutating: false`. A precondition on it would assert a version of
                // something it does not change.
                && path != bss_pricing::api::rest::history::HISTORY_EXPORT
                && !asserting.contains(&(method.clone(), path.clone()))
        })
        .map(|(method, path, _)| (method, path))
        .collect();
    silent.sort();
    silent.dedup();

    let mut stated: Vec<(String, String)> = routes_asserting_no_precondition()
        .into_iter()
        .map(|(method, path)| (method.to_owned(), path.to_owned()))
        .collect();
    stated.sort();

    assert_eq!(
        silent, stated,
        "a mutating route asserts no precondition and is not in `routes_asserting_no_precondition`, \
         or a row there names a route that now asserts one"
    );
}

#[tokio::test]
async fn the_conditional_read_declares_both_halves_or_neither() {
    // `If-None-Match` and `304` are one contract, and a route carrying one without
    // the other is the halves disagreeing: a declared header the server ignores
    // tells a client to send something that does nothing, and an undeclared 304
    // leaves a generated client parsing an empty body as the view type.
    //
    // Seven reads emitted an `ETag` and read the header on none of them until
    // 2026-08-17 — `If-None-Match` appeared nowhere in the crate — so a client that
    // had to poll for a fresh precondition re-downloaded the whole representation
    // to obtain one. The count is deliberately not asserted here; what is asserted
    // is that the two halves cannot drift apart.
    let openapi = registered_operations().await;

    let mut conditional = 0;
    for entry in &openapi.operation_specs {
        let spec = entry.value();
        let declares_header = spec.params.iter().any(|param| {
            matches!(param.location, ParamLocation::Header)
                && param.name.eq_ignore_ascii_case("if-none-match")
        });
        let declares_304 = spec.responses.iter().any(|r| r.status == 304);
        assert_eq!(
            declares_header,
            declares_304,
            "{} declares one half of the conditional read and not the other",
            entry.key()
        );
        if declares_header {
            conditional += 1;
            assert!(
                entry.key().starts_with("GET:"),
                "{} is not a read and cannot serve a conditional one",
                entry.key()
            );
        }
    }
    assert!(
        conditional >= 7,
        "the gear emits an ETag on seven reads; only {conditional} serve a conditional one"
    );
}

#[tokio::test]
async fn every_route_that_binds_an_idempotency_key_declares_a_409() {
    // `IdempotencyGate::claim` answers `IDEMPOTENCY_PAYLOAD_MISMATCH` when a spent
    // key is replayed over a changed request, and that variant maps through
    // `aborted(…)` to a **409**. So the declaration follows from the binding, and
    // this derives it rather than rostering it.
    //
    // `POST /bulk-imports` was the one route that bound a key and declared no 409
    // — the refusal that closed a data-integrity hole, invisible to every generated
    // client, while its four sibling creates all declared theirs.
    //
    // The needle is `let … = preconditions::idempotency_key`, not the call: a route
    // that reads the key for its *refusal* and binds nothing has no dedup gate to
    // conflict with, which is `abort_bulk_import`'s shape.
    let openapi = registered_operations().await;

    for (method, path) in routes_whose_handler_calls("= preconditions::idempotency_key") {
        let key = format!("{method}:{path}");
        let entry = openapi
            .operation_specs
            .get(&key)
            .unwrap_or_else(|| panic!("{key} is not a registered operation"));
        assert!(
            entry.value().responses.iter().any(|r| r.status == 409),
            "{key} binds an idempotency key and declares no 409, which a replay over a changed \
             request produces"
        );
    }
}

#[tokio::test]
async fn every_mutating_route_declares_its_precondition_header() {
    // D-171's owed clause: the declarations existed on every mutating route and
    // **no test read the registration's params**, so deleting one failed nothing. A
    // declaration is what a generated client sends; a route whose `If-Match` is
    // undeclared is one whose clients omit the header and are then refused 400 by
    // a server that never told them to send it.
    //
    // The roster is `if_match_routes()` and the count is deliberately not repeated
    // here: it said "six" against a list of seven (five `If-Match` plus the two
    // creates, which assert their precondition through the idempotency gate).
    //
    // Delete one `.param(if_match_param(...))` and exactly this assertion fails.
    let openapi = registered_operations().await;

    for (method, path) in if_match_routes() {
        let headers = declared_headers(&openapi, method, path);
        let expected = if idempotency_key_routes().contains(&(method, path)) {
            "idempotency-key"
        } else {
            "if-match"
        };
        assert!(
            headers.iter().any(|name| name == expected),
            "{method} {path} declares no {expected}: {headers:?}"
        );
    }
}

#[tokio::test]
async fn every_create_declares_its_idempotency_key() {
    // The other half, and it is a different header on a different pair of routes:
    // a create has no version to assert, so its precondition is the gate's key.
    let openapi = registered_operations().await;

    for (method, path) in idempotency_key_routes() {
        let headers = declared_headers(&openapi, method, path);
        assert!(
            headers.iter().any(|name| name == "idempotency-key"),
            "{method} {path} declares no Idempotency-Key: {headers:?}"
        );
    }
}

#[tokio::test]
async fn a_read_route_declares_no_precondition_header() {
    // The negative side, which is what stops the two assertions above from being
    // satisfiable by declaring both headers everywhere. A GET has no precondition
    // to assert, and a declared one would tell a client to send a header the
    // server ignores.
    //
    // **Derived, not a roster** (review T-3, 2026-08-19). This iterated a literal
    // array of six `GET`s against the 28 the gear registers, so it was the only
    // population claim in its group covering 21% of its population: an `If-Match`
    // added to any of the other 22 reddened nothing — not here, not in
    // `every_precondition_reading_route_declares_the_header_it_reads` (which walks
    // only routes whose *handler* calls `preconditions::if_match`), and not in
    // `the_conditional_read_declares_both_halves_or_neither` (which pairs
    // `if-none-match` with a 304). The derivation is the idiom eighty lines up:
    // key off `GET:`.
    let openapi = registered_operations().await;

    // **Both precondition headers, not `If-Match` alone.** This file treats
    // `Idempotency-Key` as the other one — `idempotency_key_routes`,
    // `every_create_declares_its_idempotency_key` — and no read handler reads it,
    // so a `GET` declaring it tells a client to send a header the server ignores
    // for the same reason and with the same cost. Keyed on `If-Match` alone this
    // scan sees a read that declares the key as compliant, on every iteration.
    //
    // `If-None-Match` is admitted and is not a precondition in this sense: it is
    // half of the conditional-read contract, and
    // `the_conditional_read_declares_both_halves_or_neither` pins it against the
    // 304 the same route must declare. Asserting `headers.is_empty()` here — the
    // shape `the_window_cancel_declares_no_precondition_header` uses for its one
    // `DELETE` — would refuse those seven reads.
    let mut offenders: Vec<String> = Vec::new();
    let mut reads = 0_usize;
    for entry in &openapi.operation_specs {
        let key = entry.key();
        let Some(path) = key.strip_prefix("GET:") else {
            continue;
        };
        reads += 1;
        let headers = declared_headers(&openapi, "GET", path);
        if headers
            .iter()
            .any(|name| name == "if-match" || name == "idempotency-key")
        {
            offenders.push(key.clone());
        }
    }
    // Anti-vacuity, and **exact** rather than a floor. `reads >= 20` was the first
    // spelling and it admitted eight of the gear's twenty-eight GETs vanishing from
    // the population: if the operation-spec scan or the `GET:` prefix ever stopped
    // matching them, this case would drop them silently and still pass -- the same
    // subset-coverage failure the roster it replaced had. `registered_handlers()` is
    // already derived in this file, so the exact operand costs nothing (2026-08-21
    // review, finding 10).
    let registered_reads = registered_handlers()
        .iter()
        .filter(|(method, _, _)| method == "GET")
        .count();
    assert_eq!(
        reads, registered_reads,
        "the GET scan found {reads} reads against the {registered_reads} the gear registers, so \
         it is not measuring the population it claims"
    );
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "these reads declare a precondition header they cannot honour: {offenders:?}"
    );
}

#[tokio::test]
async fn the_window_cancel_declares_no_precondition_header() {
    // §5's Idempotency cell for `DELETE /price-windows/{windowId}` is **empty**, and
    // `api::rest::windows` reports that as the design set's call rather than an
    // omission to improve on — the refusal of a second cancellation
    // (`WINDOW_NOT_CANCELLABLE`) is what stands in for a key.
    //
    // A **mutating** route with no declared precondition is otherwise exactly the hole
    // `every_mutating_route_declares_its_precondition_header` closes, so the absence
    // needs its own assertion or it is indistinguishable from a forgotten
    // declaration. Add a header here to be helpful and this reddens.
    let openapi = registered_operations().await;

    let headers = declared_headers(
        &openapi,
        "DELETE",
        bss_pricing::api::rest::windows::PRICE_WINDOW,
    );

    assert!(
        headers.is_empty(),
        "the cancel takes neither an If-Match nor an Idempotency-Key: {headers:?}"
    );
}

/// The **second** route that declares no precondition header, and it needs its own
/// assertion for [`the_window_cancel_declares_no_precondition_header`]'s reason.
///
/// S5's idempotency column for `POST …/supersessions` is `(planId, scope key, changeover
/// instant)` — the act's own identity — not a client key, and the handler takes no
/// `HeaderMap` at all. So a declaration either way would be a header the server ignores.
/// Without this case the absence is indistinguishable from a forgotten declaration, and a
/// later group reading the route as "a create like the other creates" could add an
/// `Idempotency-Key` param with every test still green — handing callers at-most-once
/// semantics they do not have, on a surface whose own module doc argues that wiring that
/// gate here would make the commit arm permanently unreachable.
#[tokio::test]
async fn the_supersession_declares_no_precondition_header() {
    let openapi = registered_operations().await;

    let headers = declared_headers(
        &openapi,
        "POST",
        bss_pricing::api::rest::supersessions::PLAN_SUPERSESSIONS,
    );

    assert!(
        headers.is_empty(),
        "the supersession takes neither an If-Match nor an Idempotency-Key: {headers:?}"
    );
}

/// The **third** route that declares no precondition header, and it needs its own
/// assertion for [`the_window_cancel_declares_no_precondition_header`]'s reason.
///
/// S12 §5's Idempotency cell for `POST /repricing-runs` is `run_id` — a **body
/// member**, not a header — so the surface declares neither an `Idempotency-Key`
/// nor an `If-Match`, and the handler takes no `HeaderMap` at all. Its bulk
/// sibling one path over *does* take a key, which is exactly what makes this
/// absence look like a forgotten declaration rather than a decision: a later group
/// reading the two routes as one family could add `idempotency_key_param()` here
/// with every other test green, and would then have told every generated client to
/// send a header this server never reads while the real idempotency column sat in
/// the body being ignored by the client's retry logic.
#[tokio::test]
async fn the_repricing_run_declares_no_precondition_header() {
    let openapi = registered_operations().await;

    let headers = declared_headers(
        &openapi,
        "POST",
        bss_pricing::api::rest::repricing_runs::REPRICING_RUNS,
    );

    assert!(
        headers.is_empty(),
        "the repricing run's idempotency column is its `run_id` body member, so it declares \
         neither an If-Match nor an Idempotency-Key: {headers:?}"
    );
}
