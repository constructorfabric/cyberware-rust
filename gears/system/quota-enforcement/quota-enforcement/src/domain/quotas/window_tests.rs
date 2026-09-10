#![allow(clippy::expect_used)]

use std::collections::HashMap;

use gts::GtsTypeId;
use quota_enforcement_sdk::{
    ContractRef, EnforcementMode, MetricId, MetricKind, PageResult, PeriodType, Quota, QuotaId,
    QuotaSource, QuotaStatus, QuotaType, SubjectRef, TenantId, ValidityWindow,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{view, view_page};
use crate::test_support::{LLM_USER_PROJECTION, METRIC_REQUESTS, METRIC_TOKENS};

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).expect("timestamp")
}

fn quota(metric: &str, window: Option<ValidityWindow>) -> Quota {
    Quota {
        id: QuotaId::generate(),
        tenant_id: TenantId::new(Uuid::from_u128(2)),
        subject: SubjectRef {
            projection_type: GtsTypeId::try_new(LLM_USER_PROJECTION).expect("type"),
            subject_id: "u1".to_owned(),
        },
        metric: MetricId::parse(metric).expect("metric"),
        quota_type: QuotaType::Consumption,
        period: Some(PeriodType::Month),
        enforcement_mode: EnforcementMode::Hard,
        cap: Some(10),
        notification_thresholds: Vec::new(),
        validity_window: window,
        fail_open_hint: false,
        metadata: serde_json::Map::new(),
        source: QuotaSource::Operator,
        status: QuotaStatus::Active,
        constraint_contract: ContractRef {
            type_id: GtsTypeId::new("gts.cf.core.qe.constraint.v1~x.y.z.w.v1~"),
            version: 1,
        },
        record_version: 1,
        created_at: ts(0),
        updated_at: ts(0),
    }
}

#[test]
fn the_window_is_inclusive_absent_bounds_are_open_and_status_never_moves() {
    let both = Some(ValidityWindow {
        start: Some(ts(100)),
        end: Some(ts(200)),
    });
    for (now, inside) in [(99, false), (100, true), (200, true), (201, false)] {
        let v = view(
            quota(METRIC_TOKENS, both),
            Some(MetricKind::Counter),
            ts(now),
        );
        assert_eq!(v.currently_within_window, inside, "now = {now}");
        assert_eq!(
            v.quota.status,
            QuotaStatus::Active,
            "never auto-deactivated"
        );
    }
    let start_only = Some(ValidityWindow {
        start: Some(ts(100)),
        end: None,
    });
    assert!(!view(quota(METRIC_TOKENS, start_only), None, ts(99)).currently_within_window);
    assert!(view(quota(METRIC_TOKENS, start_only), None, ts(1_000_000)).currently_within_window);
    let end_only = Some(ValidityWindow {
        start: None,
        end: Some(ts(100)),
    });
    assert!(view(quota(METRIC_TOKENS, end_only), None, ts(0)).currently_within_window);
    assert!(!view(quota(METRIC_TOKENS, end_only), None, ts(101)).currently_within_window);
    assert!(view(quota(METRIC_TOKENS, None), None, ts(0)).currently_within_window);
}

#[test]
fn a_page_is_viewed_at_one_instant_and_keeps_its_cursor() {
    let page = PageResult {
        items: vec![
            quota(
                METRIC_TOKENS,
                Some(ValidityWindow {
                    start: None,
                    end: Some(ts(50)),
                }),
            ),
            quota(METRIC_REQUESTS, None),
        ],
        next_cursor: Some("c".to_owned()),
    };
    let kinds: HashMap<MetricId, MetricKind> = HashMap::from([(
        MetricId::parse(METRIC_TOKENS).expect("metric"),
        MetricKind::Counter,
    )]);
    let viewed = view_page(page, &kinds, ts(100));
    assert_eq!(viewed.next_cursor.as_deref(), Some("c"));
    assert_eq!(viewed.items.len(), 2);
    assert!(!viewed.items[0].currently_within_window);
    assert_eq!(viewed.items[0].metric_kind, Some(MetricKind::Counter));
    assert!(viewed.items[1].currently_within_window);
    assert_eq!(
        viewed.items[1].metric_kind, None,
        "a metric absent from the kinds map is unknown, not defaulted"
    );
}
