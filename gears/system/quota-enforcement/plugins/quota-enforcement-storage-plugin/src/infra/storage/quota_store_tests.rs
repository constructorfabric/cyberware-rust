#![allow(clippy::expect_used)]

use std::sync::Arc;

use quota_enforcement_sdk::{
    CapPatch, EnforcementMode, MetricId, NotificationEvent, PageRequest, PeriodType, Quota,
    QuotaDraft, QuotaFilter, QuotaId, QuotaPatch, QuotaStatus, QuotaType, ValidityWindow,
    ValidityWindowPatch,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use time::macros::datetime;
use toolkit_db::Db;
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::secure::SecureUpdateExt;
use toolkit_security::AccessScope;

use super::SqlQuotaStore;
use crate::domain::ports::{QuotaStore, StoreError};
use crate::infra::outbox::NotificationEnqueuer;
use crate::infra::storage::entity::{operation_log, quota, quota_allocation_counter};
use crate::infra::storage::repo::operation_log_repo::{
    self, OP_QUOTA_CREATE, OP_QUOTA_DEACTIVATE, OP_QUOTA_UPDATE,
};
use crate::test_support::{
    FailingEnqueuer, METRIC_REQUESTS, actor, bound_outbox, count_rows, draft, enqueued_messages,
    other_tenant, quota_changed, scope_for, tenant, test_db, user,
};

struct Harness {
    db: Db,
    store: SqlQuotaStore,
    outbox: OutboxHandle,
}

impl Harness {
    async fn up() -> Self {
        let db = test_db().await;
        let (outbox, enqueuer) = bound_outbox(&db).await;
        let store = SqlQuotaStore::new(db.clone(), enqueuer);
        Self { db, store, outbox }
    }

    async fn down(self) {
        self.outbox.stop().await;
    }

    async fn create(&self, draft: QuotaDraft) -> QuotaId {
        let tenant = draft.tenant_id;
        self.store
            .create_quota(
                &actor(),
                &scope_for(tenant),
                draft,
                &[quota_changed(tenant)],
            )
            .await
            .expect("created")
    }

    async fn get(&self, id: QuotaId) -> Quota {
        let mut page = self
            .store
            .read_quotas(
                &AccessScope::allow_all(),
                QuotaFilter {
                    ids: vec![id],
                    ..QuotaFilter::default()
                },
                PageRequest::first(1),
            )
            .await
            .expect("read");
        page.items.pop().expect("present")
    }

    async fn update(&self, id: QuotaId, patch: QuotaPatch) -> Result<Quota, StoreError> {
        self.store
            .update_quota(
                &actor(),
                &scope_for(tenant()),
                id,
                patch,
                &[quota_changed(tenant())],
            )
            .await
    }

    async fn log(&self, id: QuotaId) -> Vec<operation_log::Model> {
        let conn = self.db.conn().expect("connection");
        operation_log_repo::entries_for_quota(&conn, &AccessScope::allow_all(), id.as_uuid())
            .await
            .expect("log")
    }

    async fn seed_in_flight(&self, id: QuotaId, in_flight: i64) {
        let conn = self.db.conn().expect("connection");
        let result = quota_allocation_counter::Entity::update_many()
            .col_expr(
                quota_allocation_counter::Column::InFlight,
                Expr::value(in_flight),
            )
            .filter(quota_allocation_counter::Column::QuotaId.eq(id.as_uuid()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("seed");
        assert_eq!(result.rows_affected, 1);
    }
}

fn consumption(subject_id: &str, cap: Option<u64>) -> QuotaDraft {
    let mut draft = draft(tenant(), subject_id, cap);
    draft.quota_type = QuotaType::Consumption;
    draft.period = Some(PeriodType::Month);
    draft
}

#[tokio::test]
async fn create_then_read_round_trips_every_column_and_writes_the_side_rows() {
    let h = Harness::up().await;
    let mut draft = draft(tenant(), "u1", Some(100));
    draft.notification_thresholds = vec![50, 90];
    draft.validity_window = Some(ValidityWindow {
        start: Some(datetime!(2026-01-01 00:00:00 UTC)),
        end: Some(datetime!(2026-12-31 23:59:59 UTC)),
    });
    draft.metadata = json!({ "regions": ["eu"], "weight": 5 })
        .as_object()
        .cloned()
        .expect("object");
    let id = h.create(draft.clone()).await;

    let quota = h.get(id).await;
    assert_eq!(quota.id, id);
    assert_eq!(quota.tenant_id, tenant());
    assert_eq!(quota.subject, user("u1"));
    assert_eq!(quota.metric, draft.metric);
    assert_eq!(quota.quota_type, QuotaType::Allocation);
    assert_eq!(quota.period, None);
    assert_eq!(quota.enforcement_mode, EnforcementMode::Hard);
    assert_eq!(quota.cap, Some(100));
    assert_eq!(quota.notification_thresholds, vec![50, 90]);
    assert_eq!(quota.validity_window, draft.validity_window);
    assert_eq!(quota.metadata, draft.metadata);
    assert_eq!(quota.source, draft.source);
    assert_eq!(quota.status, QuotaStatus::Active);
    assert_eq!(quota.constraint_contract, draft.constraint_contract);
    assert_eq!(quota.record_version, 1);
    assert_eq!(quota.created_at, quota.updated_at);

    assert_eq!(
        count_rows::<quota_allocation_counter::Entity>(&h.db).await,
        1,
        "an allocation Quota starts with its counter row"
    );
    let log = h.log(id).await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].operation, OP_QUOTA_CREATE);
    assert_eq!(log[0].actor_subject_id, actor().subject_id);
    assert_eq!(log[0].actor_subject_type, actor().subject_type);
    assert_eq!(log[0].record_version, Some(1));

    let messages = enqueued_messages(&h.db).await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].payload_type, "quota-changed");
    let event: NotificationEvent = serde_json::from_slice(&messages[0].payload).expect("json");
    assert_eq!(event.quota_id, Some(id), "the plugin fills the assigned id");
    h.down().await;
}

#[tokio::test]
async fn a_consumption_quota_gets_no_counter_row() {
    let h = Harness::up().await;
    let id = h.create(consumption("u1", Some(10))).await;
    assert_eq!(h.get(id).await.period, Some(PeriodType::Month));
    assert_eq!(
        count_rows::<quota_allocation_counter::Entity>(&h.db).await,
        0
    );
    h.down().await;
}

#[tokio::test]
async fn pagination_walks_stable_pages_in_id_order_and_bounds_its_inputs() {
    let h = Harness::up().await;
    let mut created = Vec::new();
    for i in 0..5 {
        created.push(h.create(draft(tenant(), &format!("u{i}"), Some(1))).await);
    }
    let scope = scope_for(tenant());
    let mut seen = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = h
            .store
            .read_quotas(
                &scope,
                QuotaFilter::default(),
                PageRequest { limit: 2, cursor },
            )
            .await
            .expect("page");
        pages += 1;
        seen.extend(page.items.iter().map(|q| q.id));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3, "2 + 2 + 1");
    assert_eq!(seen, created, "UUIDv7 ascending is creation order");

    let all = h
        .store
        .read_quotas(&scope, QuotaFilter::default(), PageRequest::first(0))
        .await
        .expect("default limit");
    assert_eq!(all.items.len(), 5);
    assert_eq!(all.next_cursor, None);
    let clamped = h
        .store
        .read_quotas(&scope, QuotaFilter::default(), PageRequest::first(10_000))
        .await
        .expect("clamped limit");
    assert_eq!(clamped.items.len(), 5);

    let err = h
        .store
        .read_quotas(
            &scope,
            QuotaFilter::default(),
            PageRequest {
                limit: 2,
                cursor: Some("not-a-cursor!".to_owned()),
            },
        )
        .await
        .expect_err("malformed cursor");
    assert_eq!(err, StoreError::InvalidCursor);

    let err = h
        .store
        .read_quotas(
            &scope,
            QuotaFilter {
                ids: (0..501).map(|_| QuotaId::generate()).collect(),
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect_err("too many ids");
    assert!(matches!(err, StoreError::InvalidFilter { .. }), "{err:?}");
    h.down().await;
}

#[tokio::test]
async fn filters_narrow_by_subject_metric_status_and_ids() {
    let h = Harness::up().await;
    let tokens_u1 = h.create(draft(tenant(), "u1", Some(1))).await;
    let mut requests = draft(tenant(), "u2", None);
    requests.metric = MetricId::parse(METRIC_REQUESTS).expect("metric");
    let requests_u2 = h.create(requests).await;
    h.store
        .deactivate_quota(&actor(), &scope_for(tenant()), requests_u2, &[])
        .await
        .expect("deactivated");
    let scope = scope_for(tenant());
    let ids = |page: quota_enforcement_sdk::PageResult<Quota>| {
        page.items.into_iter().map(|q| q.id).collect::<Vec<_>>()
    };

    let by_subject = h
        .store
        .read_quotas(
            &scope,
            QuotaFilter {
                subject: Some(user("u2")),
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("page");
    assert_eq!(ids(by_subject), vec![requests_u2]);
    let by_metric = h
        .store
        .read_quotas(
            &scope,
            QuotaFilter {
                metric: Some(MetricId::parse(METRIC_REQUESTS).expect("metric")),
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("page");
    assert_eq!(ids(by_metric), vec![requests_u2]);
    let active = h
        .store
        .read_quotas(
            &scope,
            QuotaFilter {
                status: Some(QuotaStatus::Active),
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("page");
    assert_eq!(ids(active), vec![tokens_u1]);
    let deactivated = h
        .store
        .read_quotas(
            &scope,
            QuotaFilter {
                status: Some(QuotaStatus::Deactivated),
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("deactivated Quotas remain readable");
    assert_eq!(ids(deactivated), vec![requests_u2]);
    let by_ids = h
        .store
        .read_quotas(
            &scope,
            QuotaFilter {
                ids: vec![tokens_u1, QuotaId::generate()],
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("page");
    assert_eq!(ids(by_ids), vec![tokens_u1]);
    h.down().await;
}

#[tokio::test]
async fn scope_is_reapplied_on_every_page_and_an_out_of_scope_draft_writes_nothing() {
    let h = Harness::up().await;
    let a1 = h.create(draft(tenant(), "a1", Some(1))).await;
    let _a2 = h.create(draft(tenant(), "a2", Some(1))).await;
    let b1 = h.create(draft(other_tenant(), "b1", Some(1))).await;

    let first = h
        .store
        .read_quotas(
            &scope_for(tenant()),
            QuotaFilter::default(),
            PageRequest::first(1),
        )
        .await
        .expect("tenant A page");
    assert_eq!(first.items[0].id, a1);
    let cursor = first.next_cursor.expect("more of A");
    let under_b = h
        .store
        .read_quotas(
            &scope_for(other_tenant()),
            QuotaFilter::default(),
            PageRequest {
                limit: 10,
                cursor: Some(cursor),
            },
        )
        .await
        .expect("A's cursor under B's scope");
    assert!(
        under_b.items.iter().all(|q| q.tenant_id == other_tenant()),
        "a cursor never grants access: {under_b:?}"
    );
    assert_eq!(
        under_b.items.iter().map(|q| q.id).collect::<Vec<_>>(),
        vec![b1]
    );

    let before = count_rows::<quota::Entity>(&h.db).await;
    let err = h
        .store
        .create_quota(
            &actor(),
            &scope_for(other_tenant()),
            draft(tenant(), "a3", Some(1)),
            &[],
        )
        .await
        .expect_err("tenant A is not in B's scope");
    assert_eq!(err, StoreError::SubjectOutOfScope);
    assert_eq!(count_rows::<quota::Entity>(&h.db).await, before);
    assert_eq!(count_rows::<operation_log::Entity>(&h.db).await, 3);

    let err = h
        .store
        .update_quota(
            &actor(),
            &scope_for(other_tenant()),
            a1,
            QuotaPatch {
                fail_open_hint: Some(true),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect_err("A's row is invisible under B");
    assert_eq!(err, StoreError::QuotaNotFound { id: a1 });
    h.down().await;
}

#[tokio::test]
async fn the_cap_guard_reads_the_in_flight_counter_under_the_row_lock() {
    let h = Harness::up().await;
    let id = h.create(draft(tenant(), "u1", Some(100))).await;
    h.seed_in_flight(id, 5).await;

    let err = h
        .update(
            id,
            QuotaPatch {
                cap: Some(CapPatch::Bounded(3)),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect_err("below consumed");
    assert_eq!(
        err,
        StoreError::CapBelowConsumed {
            new_cap: 3,
            consumed: 5
        }
    );
    assert_eq!(h.get(id).await.record_version, 1, "nothing written");
    assert_eq!(h.log(id).await.len(), 1);

    let exact = h
        .update(
            id,
            QuotaPatch {
                cap: Some(CapPatch::Bounded(5)),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect("cap equal to consumed is allowed");
    assert_eq!(exact.cap, Some(5));
    assert_eq!(exact.record_version, 2);
    let unbounded = h
        .update(
            id,
            QuotaPatch {
                cap: Some(CapPatch::Unbounded),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect("unbinding never under-caps");
    assert_eq!(unbounded.cap, None);
    assert_eq!(unbounded.record_version, 3);
    h.down().await;
}

#[tokio::test]
async fn an_update_applies_every_field_bumps_the_version_once_and_logs_field_names() {
    let h = Harness::up().await;
    let mut draft = consumption("u1", Some(100));
    draft.notification_thresholds = vec![50];
    draft.validity_window = Some(ValidityWindow {
        start: Some(datetime!(2026-01-01 00:00:00 UTC)),
        end: None,
    });
    let id = h.create(draft).await;
    let patched = h
        .update(
            id,
            QuotaPatch {
                cap: Some(CapPatch::Bounded(200)),
                notification_thresholds: Some(vec![25, 75]),
                validity_window: Some(ValidityWindowPatch::Clear),
                metadata: Some(json!({ "weight": 9 }).as_object().cloned().expect("object")),
                constraint_contract: Some(contract_v2()),
                enforcement_mode: Some(EnforcementMode::Hard),
                fail_open_hint: Some(true),
            },
        )
        .await
        .expect("updated");
    assert_eq!(patched.cap, Some(200));
    assert_eq!(patched.notification_thresholds, vec![25, 75]);
    assert_eq!(patched.validity_window, None);
    assert_eq!(patched.metadata["weight"], json!(9));
    assert_eq!(
        patched.constraint_contract,
        contract_v2(),
        "the reference moves with the metadata"
    );
    assert!(patched.fail_open_hint);
    assert_eq!(patched.record_version, 2);
    assert!(patched.updated_at >= patched.created_at);
    assert_eq!(
        h.get(id).await,
        patched,
        "the returned row is the committed row"
    );

    let log = h.log(id).await;
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].operation, OP_QUOTA_UPDATE);
    assert_eq!(log[1].record_version, Some(2));
    assert_eq!(
        log[1].detail,
        "cap,notification_thresholds,validity_window,metadata,constraint_contract,enforcement_mode,fail_open_hint"
    );
    assert_eq!(enqueued_messages(&h.db).await.len(), 2);

    let err = h
        .update(QuotaId::generate(), QuotaPatch::default())
        .await
        .expect_err("unknown id");
    assert!(matches!(err, StoreError::QuotaNotFound { .. }), "{err:?}");

    let err = h
        .update(
            id,
            QuotaPatch {
                metadata: Some(json!({ "weight": 1 }).as_object().cloned().expect("object")),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect_err("a metadata patch without its contract is refused before any write");
    assert!(matches!(err, StoreError::InvalidPatch { .. }), "{err:?}");
    assert_eq!(h.get(id).await, patched, "nothing written");
    assert_eq!(h.log(id).await.len(), 2);
    h.down().await;
}

fn contract_v2() -> quota_enforcement_sdk::ContractRef {
    quota_enforcement_sdk::ContractRef {
        type_id: gts::GtsTypeId::new(
            "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v2~",
        ),
        version: 2,
    }
}

#[tokio::test]
async fn thresholds_on_an_unbounded_cap_are_refused_inside_the_transaction() {
    let h = Harness::up().await;
    let mut draft = consumption("u1", Some(100));
    draft.notification_thresholds = vec![50];
    let id = h.create(draft).await;

    let err = h
        .update(
            id,
            QuotaPatch {
                cap: Some(CapPatch::Unbounded),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect_err("I14 on the merged row: stored thresholds, patched cap");
    assert_eq!(err, StoreError::ThresholdsRequireBoundedCap);
    assert_eq!(h.get(id).await.record_version, 1);

    let cleared = h
        .update(
            id,
            QuotaPatch {
                cap: Some(CapPatch::Unbounded),
                notification_thresholds: Some(vec![]),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect("unbinding with the thresholds cleared");
    assert_eq!(cleared.cap, None);
    assert!(cleared.notification_thresholds.is_empty());

    let err = h
        .update(
            id,
            QuotaPatch {
                notification_thresholds: Some(vec![10]),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect_err("I14 on the merged row: stored unbounded cap, patched thresholds");
    assert_eq!(err, StoreError::ThresholdsRequireBoundedCap);
    assert_eq!(h.get(id).await.record_version, 2);
    h.down().await;
}

#[tokio::test]
async fn deactivation_flips_the_status_once_and_is_terminal() {
    let h = Harness::up().await;
    let id = h.create(draft(tenant(), "u1", Some(100))).await;
    let outcome = h
        .store
        .deactivate_quota(
            &actor(),
            &scope_for(tenant()),
            id,
            &[quota_changed(tenant())],
        )
        .await
        .expect("deactivated");
    assert!(
        outcome.resolved_leases.is_empty(),
        "the cascade lands with leases"
    );
    let quota = h.get(id).await;
    assert_eq!(quota.status, QuotaStatus::Deactivated);
    assert_eq!(quota.record_version, 2);

    let again = h
        .store
        .deactivate_quota(
            &actor(),
            &scope_for(tenant()),
            id,
            &[quota_changed(tenant())],
        )
        .await
        .expect_err("second deactivation");
    assert_eq!(again, StoreError::QuotaDeactivated { id });
    let update = h
        .update(
            id,
            QuotaPatch {
                fail_open_hint: Some(true),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect_err("deactivated rows accept no patch");
    assert_eq!(update, StoreError::QuotaDeactivated { id });

    let log = h.log(id).await;
    assert_eq!(log.len(), 2, "the refused calls wrote nothing");
    assert_eq!(log[1].operation, OP_QUOTA_DEACTIVATE);
    assert_eq!(log[1].record_version, Some(2));
    assert_eq!(enqueued_messages(&h.db).await.len(), 2);
    let unknown = h
        .store
        .deactivate_quota(&actor(), &scope_for(tenant()), QuotaId::generate(), &[])
        .await
        .expect_err("unknown");
    assert!(
        matches!(unknown, StoreError::QuotaNotFound { .. }),
        "{unknown:?}"
    );
    h.down().await;
}

#[tokio::test]
async fn bindings_and_counts_cover_active_rows_only() {
    let h = Harness::up().await;
    h.create(draft(tenant(), "u1", Some(0))).await;
    h.create(draft(tenant(), "u2", None)).await;
    h.create(draft(other_tenant(), "u3", Some(7))).await;
    let mut requests = draft(tenant(), "u4", None);
    requests.metric = MetricId::parse(METRIC_REQUESTS).expect("metric");
    let gone = h.create(requests).await;
    h.store
        .deactivate_quota(&actor(), &scope_for(tenant()), gone, &[])
        .await
        .expect("deactivated");

    let bindings = h
        .store
        .read_active_projection_bindings()
        .await
        .expect("bindings");
    assert_eq!(bindings.len(), 1, "distinct over active rows: {bindings:?}");
    let binding = bindings.iter().next().expect("one");
    assert_eq!(
        binding.metric.as_str(),
        draft(tenant(), "x", None).metric.as_str()
    );
    assert_eq!(binding.projection_type, user("x").projection_type);

    let counts = h.store.read_active_quota_counts().await.expect("counts");
    assert_eq!(counts.cap_zero, 1);
    assert_eq!(
        counts.cap_unbounded, 1,
        "the deactivated unbounded row is not counted"
    );
    assert_eq!(counts.by_metric.len(), 1);
    assert_eq!(
        counts.by_metric.get(&draft(tenant(), "x", None).metric),
        Some(&3),
        "platform-wide, both tenants"
    );
    h.down().await;
}

#[tokio::test]
async fn a_failing_or_unbound_enqueue_rolls_back_every_row() {
    let db = test_db().await;
    let failing = SqlQuotaStore::new(db.clone(), Arc::new(FailingEnqueuer));
    let err = failing
        .create_quota(
            &actor(),
            &scope_for(tenant()),
            draft(tenant(), "u1", Some(1)),
            &[quota_changed(tenant())],
        )
        .await
        .expect_err("enqueue fails");
    assert!(matches!(err, StoreError::Corrupt { .. }), "{err:?}");

    let unbound: Arc<dyn NotificationEnqueuer> = Arc::new(crate::infra::outbox::QeOutbox::new());
    let store = SqlQuotaStore::new(db.clone(), unbound);
    let err = store
        .create_quota(
            &actor(),
            &scope_for(tenant()),
            draft(tenant(), "u1", Some(1)),
            &[quota_changed(tenant())],
        )
        .await
        .expect_err("outbox not bound");
    assert_eq!(
        err,
        StoreError::Unavailable {
            operation: "create quota"
        }
    );

    assert_eq!(count_rows::<quota::Entity>(&db).await, 0);
    assert_eq!(count_rows::<quota_allocation_counter::Entity>(&db).await, 0);
    assert_eq!(count_rows::<operation_log::Entity>(&db).await, 0);
    assert!(enqueued_messages(&db).await.is_empty());
}
