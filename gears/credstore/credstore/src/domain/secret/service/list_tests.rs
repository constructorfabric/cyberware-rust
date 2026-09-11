//! Unit tests for `Service::list` (ADR-0005/ADR-0004 collection read).

use std::sync::Arc;

use credstore_sdk::{
    CredentialPatch, CredentialStatus, CredentialWrite, Fallback as SdkFallback, InheritanceStatus,
    PatchField, SecretRef, SecretType, SecretValue, SharingMode,
};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::metrics::NoopMetrics;
use crate::domain::ports::plugin::PluginSelector;
use crate::domain::resolver::TenantDirectory;
use crate::domain::secret::repo::SecretRepo;
use crate::domain::secret::service::{GcSettings, ListSettings, Service};
use crate::domain::secret::test_support::*;

fn key(s: &str) -> SecretRef {
    SecretRef::new(s).expect("valid ref")
}

fn create_only() -> crate::domain::secret::model::PutPrecondition {
    crate::domain::secret::model::PutPrecondition::CreateOnly
}

fn write_typed(sharing: SharingMode, value: &str, type_name: &str) -> CredentialWrite {
    CredentialWrite {
        secret_type: Some(SecretType::from_name(type_name).expect("known type").into()),
        sharing,
        fallback: SdkFallback::Inherit,
        expires_at: None,
        value: SecretValue::from(value),
    }
}

fn write_generic(sharing: SharingMode, value: &str) -> CredentialWrite {
    write_typed(sharing, value, "generic")
}

fn patch_suppress() -> CredentialPatch {
    CredentialPatch {
        secret_type: None,
        sharing: None,
        fallback: Some(SdkFallback::None),
        expires_at: PatchField::Absent,
        value: PatchField::Null,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "test builder threading every Service::new dependency plus list settings through"
)]
fn service_with(
    repo: Arc<dyn SecretRepo>,
    plugin: Arc<FakePlugin>,
    dir: Arc<dyn TenantDirectory>,
    enforcer: authz_resolver_sdk::PolicyEnforcer,
    max_limit: u64,
    value_mode_cap: u64,
) -> Service {
    Service::new(
        repo,
        dir,
        enforcer,
        Arc::new(FakePluginSelector::new(plugin)) as Arc<dyn PluginSelector>,
        catalog_type_resolver(),
        Arc::new(NoopMetrics),
        GcSettings {
            pending_max_age_secs: 3600,
            batch_size: 256,
        },
        ListSettings {
            max_limit,
            value_mode_cap,
        },
    )
}

fn default_service(repo: Arc<dyn SecretRepo>, dir: Arc<dyn TenantDirectory>) -> Service {
    service_with(repo, FakePlugin::new(), dir, mock_enforcer(), 200, 25)
}

fn references_of(page: &toolkit_odata::Page<credstore_sdk::CredentialListItem>) -> Vec<String> {
    page.items
        .iter()
        .map(|i| i.credential.reference.as_ref().to_owned())
        .collect()
}

fn filter_expr(raw: &str) -> toolkit_odata::ast::Expr {
    toolkit_odata::parse_filter_string(raw)
        .expect("valid OData syntax")
        .into_expr()
}

fn reason_of(err: &DomainError) -> &'static str {
    match err {
        DomainError::InvalidRequest { reason, .. } => reason,
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

// ── metadata mode: own / inherited / overridden / suppressed ────────────────

#[tokio::test]
async fn metadata_page_reports_own_inherited_overridden_and_suppressed() {
    let t1 = Uuid::new_v4();
    let t3 = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir_t1 = Arc::new(FakeDir::single(t1));
    let dir_t3 = Arc::new(FakeDir::new(vec![t3, t1]));
    let enforcer = mock_enforcer();
    let svc_t1 = service_with(
        repo.clone(),
        plugin.clone(),
        dir_t1,
        enforcer.clone(),
        200,
        25,
    );
    let svc_t3 = service_with(repo.clone(), plugin.clone(), dir_t3, enforcer, 200, 25);

    let owner1 = Uuid::new_v4();
    let ctx1 = make_ctx(owner1, t1);
    let owner3 = Uuid::new_v4();
    let ctx3 = make_ctx(owner3, t3);

    // a-own: only T3 holds a row.
    svc_t3
        .put(
            &ctx3,
            &key("a-own"),
            write_generic(SharingMode::Tenant, "v-a"),
            create_only(),
        )
        .await
        .expect("a-own create");

    // b-inherited: only T1 (shared); T3 holds nothing.
    svc_t1
        .put(
            &ctx1,
            &key("b-inherited"),
            write_generic(SharingMode::Shared, "v-b"),
            create_only(),
        )
        .await
        .expect("b-inherited create");

    // c-overridden: T1 shares, T3 shadows with its own.
    svc_t1
        .put(
            &ctx1,
            &key("c-overridden"),
            write_generic(SharingMode::Shared, "v-c-parent"),
            create_only(),
        )
        .await
        .expect("c-overridden parent create");
    svc_t3
        .put(
            &ctx3,
            &key("c-overridden"),
            write_generic(SharingMode::Tenant, "v-c-own"),
            create_only(),
        )
        .await
        .expect("c-overridden own create");

    // d-suppressed: T1 shares, T3 suppresses.
    svc_t1
        .put(
            &ctx1,
            &key("d-suppressed"),
            write_generic(SharingMode::Shared, "v-d-parent"),
            create_only(),
        )
        .await
        .expect("d-suppressed parent create");
    svc_t3
        .put(
            &ctx3,
            &key("d-suppressed"),
            write_generic(SharingMode::Tenant, "v-d-own"),
            create_only(),
        )
        .await
        .expect("d-suppressed own create");
    let existing = svc_t3
        .get(&ctx3, &key("d-suppressed"))
        .await
        .expect("get")
        .expect("own row");
    svc_t3
        .patch(
            &ctx3,
            &key("d-suppressed"),
            patch_suppress(),
            crate::domain::secret::model::WritePrecondition::Version {
                id: existing.validator.expect("own validator").id,
                version: existing.validator.expect("own validator").version,
            },
        )
        .await
        .expect("suppress");

    let page = svc_t3.list(&ctx3, &ODataQuery::new()).await.expect("list");

    assert_eq!(
        references_of(&page),
        vec!["a-own", "b-inherited", "c-overridden", "d-suppressed"]
    );

    let by_ref = |r: &str| {
        page.items
            .iter()
            .find(|i| i.credential.reference.as_ref() == r)
            .unwrap_or_else(|| panic!("{r} missing from page"))
    };

    let a = by_ref("a-own");
    assert_eq!(a.credential.inheritance, InheritanceStatus::Own);
    assert!(a.credential.owner_id.is_some());
    assert_eq!(a.credential.status, CredentialStatus::Active);

    let b = by_ref("b-inherited");
    assert_eq!(b.credential.inheritance, InheritanceStatus::Inherited);
    assert!(b.credential.owner_id.is_none());
    assert_eq!(b.credential.status, CredentialStatus::None);

    let c = by_ref("c-overridden");
    assert_eq!(c.credential.inheritance, InheritanceStatus::Overridden);
    assert!(c.credential.owner_id.is_some());

    let d = by_ref("d-suppressed");
    assert_eq!(d.credential.inheritance, InheritanceStatus::Suppressed);
    assert!(d.credential.owner_id.is_some());
    assert_eq!(d.credential.status, CredentialStatus::Declared);

    // Metadata mode never carries a value.
    assert!(page.items.iter().all(|i| i.secret.is_none()));
}

// ── type clamp / authorization ───────────────────────────────────────────────

#[tokio::test]
async fn a_denied_types_references_never_appear() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let denied_gts = SecretType::from_name("api-key")
        .expect("known")
        .gts_id()
        .to_owned();
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    // Create both credentials under a permissive enforcer: `type_deny_enforcer`
    // denies every action on the api-key type, including the `write`/
    // `write_secret` the create itself would need.
    let svc_setup = service_with(
        repo.clone(),
        plugin.clone(),
        dir.clone(),
        mock_enforcer(),
        200,
        25,
    );
    svc_setup
        .put(
            &ctx,
            &key("allowed-generic"),
            write_generic(SharingMode::Tenant, "v1"),
            create_only(),
        )
        .await
        .expect("create generic");
    svc_setup
        .put(
            &ctx,
            &key("denied-api-key"),
            write_typed(SharingMode::Tenant, "v2", "api-key"),
            create_only(),
        )
        .await
        .expect("create api-key");

    let (enforcer, _resolver) = type_deny_enforcer(vec![denied_gts]);
    let svc = service_with(repo, plugin, dir, enforcer, 200, 25);
    let page = svc.list(&ctx, &ODataQuery::new()).await.expect("list");
    assert_eq!(references_of(&page), vec!["allowed-generic"]);
}

#[tokio::test]
async fn reference_in_and_type_eq_clamp_the_result() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = default_service(repo, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    for (name, type_name) in [("r1", "generic"), ("r2", "generic"), ("r3", "api-key")] {
        svc.put(
            &ctx,
            &key(name),
            write_typed(SharingMode::Tenant, "v", type_name),
            create_only(),
        )
        .await
        .expect("create");
    }

    let by_ref_in = ODataQuery::new().with_filter(filter_expr("reference in ('r1', 'r3')"));
    let page = svc.list(&ctx, &by_ref_in).await.expect("list");
    assert_eq!(references_of(&page), vec!["r1", "r3"]);

    let generic_gts = SecretType::from_name("generic")
        .expect("known")
        .gts_id()
        .to_owned();
    let by_type = ODataQuery::new().with_filter(filter_expr(&format!("type eq '{generic_gts}'")));
    let page = svc.list(&ctx, &by_type).await.expect("list");
    assert_eq!(references_of(&page), vec!["r1", "r2"]);
}

#[tokio::test]
async fn sharing_filter_is_applied_after_reduction() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = default_service(repo, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("shared-one"),
        write_generic(SharingMode::Shared, "v"),
        create_only(),
    )
    .await
    .expect("create shared");
    svc.put(
        &ctx,
        &key("tenant-one"),
        write_generic(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create tenant");

    let query = ODataQuery::new().with_filter(filter_expr("sharing eq 'shared'"));
    let page = svc.list(&ctx, &query).await.expect("list");
    assert_eq!(references_of(&page), vec!["shared-one"]);
}

// ── pagination ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn cursor_round_trip_across_two_pages_has_no_duplicates_or_gaps() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = default_service(repo, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let names = ["r1", "r2", "r3", "r4", "r5"];
    for name in names {
        svc.put(
            &ctx,
            &key(name),
            write_generic(SharingMode::Tenant, "v"),
            create_only(),
        )
        .await
        .expect("create");
    }

    let mut seen = Vec::new();
    let mut query = ODataQuery::new().with_limit(2);
    loop {
        let page = svc.list(&ctx, &query).await.expect("list");
        seen.extend(references_of(&page));
        match &page.page_info.next_cursor {
            Some(token) => {
                let cursor = toolkit_odata::CursorV1::decode(token).expect("decodable cursor");
                query = ODataQuery::new().with_limit(2).with_cursor(cursor);
            }
            None => break,
        }
        assert!(
            seen.len() <= names.len() + 1,
            "pagination did not terminate"
        );
    }

    assert_eq!(seen, names.to_vec(), "no duplicates, no gaps, in order");
}

#[tokio::test]
async fn orderby_other_than_reference_is_rejected() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = default_service(repo, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let query = ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
        field: "updated_at".to_owned(),
        dir: SortDir::Asc,
    }]));
    let err = svc.list(&ctx, &query).await.expect_err("must be rejected");
    assert_eq!(reason_of(&err), "INVALID_ORDERBY_FIELD");
}

// ── value mode ───────────────────────────────────────────────────────────────

fn value_mode_query(filter_raw: &str) -> ODataQuery {
    ODataQuery::new()
        .with_select(vec!["reference".to_owned(), "secret".to_owned()])
        .with_filter(filter_expr(filter_raw))
}

#[tokio::test]
async fn value_mode_rejects_limit_and_cursor() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = default_service(repo, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let with_limit = value_mode_query("reference eq 'r'").with_limit(10);
    let err = svc
        .list(&ctx, &with_limit)
        .await
        .expect_err("limit must be rejected in value mode");
    assert_eq!(reason_of(&err), "VALUE_MODE_NO_PAGINATION");
}

#[tokio::test]
async fn value_mode_over_cap_fails_closed() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = service_with(repo, plugin, dir, mock_enforcer(), 200, 2);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    for name in ["r1", "r2", "r3"] {
        svc.put(
            &ctx,
            &key(name),
            write_generic(SharingMode::Tenant, "v"),
            create_only(),
        )
        .await
        .expect("create");
    }

    let query = value_mode_query("reference in ('r1', 'r2', 'r3')");
    let err = svc.list(&ctx, &query).await.expect_err("must fail closed");
    assert_eq!(reason_of(&err), "TOO_MANY_MATCHES");
}

#[tokio::test]
async fn value_mode_items_carry_values_and_evaluate_read_secret_once_per_type() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let (enforcer, resolver) = type_recording_enforcer();
    let svc = service_with(repo, plugin, dir, enforcer, 200, 25);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("r1"),
        write_generic(SharingMode::Tenant, "value-one"),
        create_only(),
    )
    .await
    .expect("create r1");
    svc.put(
        &ctx,
        &key("r2"),
        write_generic(SharingMode::Tenant, "value-two"),
        create_only(),
    )
    .await
    .expect("create r2");

    let query = value_mode_query("reference in ('r1', 'r2')");
    let page = svc.list(&ctx, &query).await.expect("list");

    assert_eq!(page.page_info.next_cursor, None);
    assert_eq!(page.items.len(), 2);
    for item in &page.items {
        let value = item.secret.as_ref().expect("value present");
        let expected = if item.credential.reference.as_ref() == "r1" {
            "value-one"
        } else {
            "value-two"
        };
        assert_eq!(value.as_bytes(), expected.as_bytes());
    }

    // Both `r1`/`r2` share the `generic` type, so `read_secret` is
    // evaluated exactly once for it, not once per item.
    let read_secret_evals = resolver
        .seen_actions()
        .into_iter()
        .filter(|a| a == "read_secret")
        .count();
    assert_eq!(read_secret_evals, 1);
}

#[tokio::test]
async fn value_mode_omits_items_of_a_refused_type() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let denied_gts = SecretType::from_name("api-key")
        .expect("known")
        .gts_id()
        .to_owned();
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let svc_setup = service_with(
        repo.clone(),
        plugin.clone(),
        dir.clone(),
        mock_enforcer(),
        200,
        25,
    );
    svc_setup
        .put(
            &ctx,
            &key("allowed"),
            write_generic(SharingMode::Tenant, "v-ok"),
            create_only(),
        )
        .await
        .expect("create allowed");
    svc_setup
        .put(
            &ctx,
            &key("denied"),
            write_typed(SharingMode::Tenant, "v-no", "api-key"),
            create_only(),
        )
        .await
        .expect("create denied");

    let (enforcer, _resolver) = type_deny_enforcer(vec![denied_gts]);
    let svc = service_with(repo, plugin, dir, enforcer, 200, 25);
    let query = value_mode_query("reference in ('allowed', 'denied')");
    let page = svc.list(&ctx, &query).await.expect("list");
    assert_eq!(references_of(&page), vec!["allowed"]);
    assert_eq!(
        page.items[0].secret.as_ref().expect("value").as_bytes(),
        b"v-ok"
    );
}
