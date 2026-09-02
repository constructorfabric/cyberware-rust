use std::collections::BTreeMap;

use gts::GtsTypeId;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    BootstrapBundle, CapPatch, ConfigDefaults, Decision, DecisionResult, EnforcementMode,
    IdempotencySubjectKey, LeaseState, MetricId, NotificationEventKind, OperationType, PageRequest,
    PageResult, PeriodType, PolicyId, PolicyScope, QuotaDebitPlan, QuotaId, QuotaPatch,
    QuotaSource, QuotaType, SubjectRef, UnknownValue, ValidityWindow,
};
use crate::storage_plugin::CONTRACT_MAJOR;

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).expect("valid unix timestamp")
}

// --- GTS-anchored closed enums ---------------------------------------------

#[test]
fn quota_type_round_trips_through_its_gts_instance_id() {
    for value in QuotaType::ALL {
        let id = value.as_gts_id();
        assert!(id.starts_with(QuotaType::BASE_TYPE_ID), "{id}");
        assert!(!id.ends_with('~'), "instance ids never end with '~': {id}");
        let parsed: QuotaType = id.parse().expect("parse back");
        assert_eq!(parsed, *value);
        let json = serde_json::to_string(value).expect("serialize");
        assert_eq!(json, format!("\"{id}\""));
        let back: QuotaType = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, *value);
    }
}

#[test]
fn gts_enums_reject_short_names_and_unknown_ids() {
    let err = "allocation"
        .parse::<QuotaType>()
        .expect_err("short name rejected");
    assert_eq!(
        err,
        UnknownValue {
            kind: "quota_type",
            value: "allocation".to_owned(),
        }
    );
    assert!(
        "gts.cf.qe.quota.type.v1~cf.qe.quota.burst.v1"
            .parse::<QuotaType>()
            .is_err()
    );
    assert!(
        serde_json::from_str::<EnforcementMode>("\"hard\"").is_err(),
        "enforcement mode must be the full GTS id"
    );
    assert!("".parse::<PeriodType>().is_err());
}

#[test]
fn every_gts_enum_value_derives_from_its_declared_base() {
    let cases: Vec<(&str, Vec<&str>)> = vec![
        (
            EnforcementMode::BASE_TYPE_ID,
            EnforcementMode::ALL.iter().map(|v| v.as_gts_id()).collect(),
        ),
        (
            QuotaSource::BASE_TYPE_ID,
            QuotaSource::ALL.iter().map(|v| v.as_gts_id()).collect(),
        ),
        (
            PeriodType::BASE_TYPE_ID,
            PeriodType::ALL.iter().map(|v| v.as_gts_id()).collect(),
        ),
    ];
    for (base, ids) in cases {
        assert!(base.ends_with('~'), "base must be a type id: {base}");
        for id in &ids {
            assert!(id.starts_with(base), "{id} must derive from {base}");
        }
        let mut dedup = ids.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(dedup.len(), ids.len(), "duplicate ids under {base}");
    }
    assert_eq!(PeriodType::ALL.len(), 5, "PRD 5.4 reserves five periods");
    assert_eq!(
        PeriodType::OneTime.as_gts_id(),
        "gts.cf.qe.period.type.v1~cf.qe.period.one_time.v1"
    );
}

// --- plain closed enums ----------------------------------------------------

#[test]
fn notification_event_kinds_serialize_as_the_prd_kebab_case_catalog() {
    let cases = vec![
        (NotificationEventKind::ThresholdCrossed, "threshold-crossed"),
        (NotificationEventKind::PeriodRollover, "period-rollover"),
        (
            NotificationEventKind::LeaseAutoReleased,
            "lease-auto-released",
        ),
        (
            NotificationEventKind::LeaseResolvedByDeactivation,
            "lease-resolved-by-deactivation",
        ),
        (NotificationEventKind::QuotaChanged, "quota-changed"),
        (
            NotificationEventKind::QuotaCounterAdjusted,
            "quota-counter-adjusted",
        ),
        (
            NotificationEventKind::QuotaRollbackApplied,
            "quota-rollback-applied",
        ),
        (NotificationEventKind::PolicyChanged, "policy-changed"),
    ];
    for (kind, wire) in cases {
        assert_eq!(serde_json::to_value(kind).expect("serialize"), json!(wire));
        let back: NotificationEventKind = serde_json::from_value(json!(wire)).expect("parse");
        assert_eq!(back, kind);
    }
    assert!(serde_json::from_value::<NotificationEventKind>(json!("quota-deleted")).is_err());
}

#[test]
fn operation_type_names_match_the_storage_discriminator() {
    for op in [
        OperationType::Debit,
        OperationType::Credit,
        OperationType::Rollback,
        OperationType::Reserve,
        OperationType::Commit,
        OperationType::Release,
        OperationType::BatchDebit,
    ] {
        let wire = serde_json::to_value(op).expect("serialize");
        assert_eq!(
            wire,
            json!(op.as_str()),
            "serde and as_str must agree for {op:?}"
        );
    }
    assert_eq!(OperationType::BatchDebit.as_str(), "batch_debit");
}

#[test]
fn only_the_active_lease_state_is_non_terminal() {
    assert!(!LeaseState::Active.is_terminal());
    for terminal in [
        LeaseState::Committed,
        LeaseState::Released,
        LeaseState::AutoReleased,
        LeaseState::ResolvedByDeactivation,
    ] {
        assert!(terminal.is_terminal(), "{terminal:?} must be terminal");
    }
}

// --- identifiers and digests -----------------------------------------------

#[test]
fn generated_quota_ids_are_time_ordered_uuid_v7() {
    let a = QuotaId::generate();
    let b = QuotaId::generate();
    assert_eq!(a.as_uuid().get_version_num(), 7);
    assert!(a <= b, "UUIDv7 ids generated in sequence must not decrease");
    let raw = Uuid::from_u128(7);
    assert_eq!(QuotaId::new(raw).to_string(), raw.to_string());
    assert_eq!(Uuid::from(QuotaId::from(raw)), raw);
}

#[test]
fn policy_id_recognizes_the_seeded_global_policy() {
    assert!(PolicyId::global().is_global());
    assert!(!PolicyId::new("metric-a").is_global());
    let json = serde_json::to_string(&PolicyId::global()).expect("serialize");
    assert_eq!(json, "\"global\"");
}

#[test]
fn metric_id_accepts_instance_ids_and_rejects_type_ids() {
    let ok = MetricId::parse("gts.cf.qe.metric.type.v1~cf.genai.llm_gateway.token.v1");
    assert!(ok.is_ok(), "{ok:?}");
    assert!(
        MetricId::parse("gts.cf.qe.metric.type.v1~").is_err(),
        "type id rejected"
    );
    assert!(MetricId::parse("not-a-gts-id").is_err());
    let metric = ok.expect("parsed");
    let json = serde_json::to_string(&metric).expect("serialize");
    assert_eq!(json, format!("\"{}\"", metric.as_str()));
}

#[test]
fn digest_hex_round_trip_and_rejection() {
    let mut bytes = [0_u8; 32];
    bytes[0] = 0xab;
    bytes[31] = 0x01;
    let key = IdempotencySubjectKey::from_bytes(bytes);
    let hex = key.to_hex();
    assert_eq!(hex.len(), 64);
    assert!(hex.starts_with("ab") && hex.ends_with("01"));
    assert_eq!(IdempotencySubjectKey::parse_hex(&hex).expect("parse"), key);
    assert_eq!(
        IdempotencySubjectKey::parse_hex(&hex.to_uppercase()).expect("uppercase"),
        key
    );
    let json = serde_json::to_string(&key).expect("serialize");
    assert_eq!(json, format!("\"{hex}\""));
    for bad in ["", "abc", &"zz".repeat(32), &"ab".repeat(31)] {
        assert!(
            IdempotencySubjectKey::parse_hex(bad).is_err(),
            "must reject {bad:?}"
        );
    }
    assert!(
        format!("{key:?}").contains(&hex),
        "debug must show the hex form"
    );
}

// --- structs ---------------------------------------------------------------

#[test]
fn validity_window_bounds_are_inclusive_and_optional() {
    let both = ValidityWindow {
        start: Some(ts(100)),
        end: Some(ts(200)),
    };
    assert!(both.contains(ts(100)));
    assert!(both.contains(ts(200)));
    assert!(!both.contains(ts(99)));
    assert!(!both.contains(ts(201)));
    let open_end = ValidityWindow {
        start: Some(ts(100)),
        end: None,
    };
    assert!(open_end.contains(ts(1_000_000)));
    assert!(!open_end.contains(ts(0)));
    assert!(ValidityWindow::default().contains(ts(0)));
}

#[test]
fn validity_window_serializes_rfc3339_and_rejects_unknown_fields() {
    let window = ValidityWindow {
        start: Some(ts(0)),
        end: None,
    };
    let value = serde_json::to_value(window).expect("serialize");
    assert_eq!(
        value,
        json!({ "start": "1970-01-01T00:00:00Z", "end": null })
    );
    let back: ValidityWindow = serde_json::from_value(value).expect("deserialize");
    assert_eq!(back, window);
    let bad = serde_json::from_value::<ValidityWindow>(json!({ "start": null, "until": 1 }));
    assert!(bad.is_err(), "unknown fields are rejected");
}

#[test]
fn quota_patch_default_is_empty_and_any_field_makes_it_non_empty() {
    assert!(QuotaPatch::default().is_empty());
    let patch = QuotaPatch {
        cap: Some(CapPatch::Unbounded),
        ..QuotaPatch::default()
    };
    assert!(!patch.is_empty());
    let parsed: QuotaPatch =
        serde_json::from_value(json!({ "cap": { "bounded": 10 } })).expect("parse patch");
    assert_eq!(parsed.cap, Some(CapPatch::Bounded(10)));
    assert!(serde_json::from_value::<QuotaPatch>(json!({ "metric": "x" })).is_err());
}

#[test]
fn decision_result_is_tagged_and_denied_carries_its_reason() {
    let quota = QuotaId::new(Uuid::from_u128(1));
    let denied = Decision {
        result: DecisionResult::Denied {
            violated_quota_ids: vec![quota],
            reason: "NO_APPLICABLE_QUOTA".to_owned(),
        },
        debit_plan: BTreeMap::new(),
        diagnostics: BTreeMap::new(),
    };
    let value = serde_json::to_value(&denied).expect("serialize");
    assert_eq!(value["result"]["outcome"], json!("denied"));
    assert_eq!(value["result"]["reason"], json!("NO_APPLICABLE_QUOTA"));
    let back: Decision = serde_json::from_value(value).expect("deserialize");
    assert_eq!(back, denied);

    let allowed = Decision {
        result: DecisionResult::Allowed,
        debit_plan: BTreeMap::from([(quota, QuotaDebitPlan { amount: 5 })]),
        diagnostics: BTreeMap::new(),
    };
    let value = serde_json::to_value(&allowed).expect("serialize");
    assert_eq!(value["result"], json!({ "outcome": "allowed" }));
    assert_eq!(value["debit_plan"][quota.to_string()]["amount"], json!(5));
}

#[test]
fn policy_scope_is_tagged_by_kind() {
    let metric = MetricId::parse("gts.cf.qe.metric.type.v1~cf.genai.llm_gateway.token.v1")
        .expect("metric id");
    let scope = PolicyScope::Metric {
        metric: metric.clone(),
    };
    let value = serde_json::to_value(&scope).expect("serialize");
    assert_eq!(value["kind"], json!("metric"));
    assert_eq!(value["metric"], json!(metric.as_str()));
    assert_eq!(
        serde_json::to_value(PolicyScope::Global).expect("serialize"),
        json!({ "kind": "global" })
    );
}

#[test]
fn page_types_default_to_the_platform_page_size_and_map_items() {
    let request = PageRequest::default();
    assert_eq!(request.limit, PageRequest::DEFAULT_LIMIT);
    assert_eq!(request.limit, 100, "PRD 5.10 default page size");
    assert!(request.cursor.is_none());
    let page = PageResult {
        items: vec![1_u32, 2, 3],
        next_cursor: Some("c".to_owned()),
    };
    let mapped = page.map(|n| n * 10);
    assert_eq!(mapped.items, vec![10, 20, 30]);
    assert_eq!(mapped.next_cursor.as_deref(), Some("c"));
    assert!(PageResult::<u8>::empty().items.is_empty());
}

#[test]
fn foundation_bundle_carries_the_contract_major_and_prd_defaults() {
    let bundle = BootstrapBundle::foundation();
    assert_eq!(bundle.contract_major, CONTRACT_MAJOR);
    assert!(bundle.global_policy.is_none(), "seeded by a later feature");
    assert_eq!(
        bundle.config_defaults,
        ConfigDefaults {
            contention_timeout_ms: 0,
            max_active_leases: 1000,
            idempotency_retention_secs: 86_400,
        }
    );
}

#[test]
fn subject_ref_equality_covers_both_halves_of_the_identity() {
    let projection = GtsTypeId::new("gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~");
    let a = SubjectRef {
        projection_type: projection.clone(),
        subject_id: "u1".to_owned(),
    };
    let b = SubjectRef {
        projection_type: projection,
        subject_id: "u2".to_owned(),
    };
    assert_ne!(a, b);
    assert_eq!(a.clone(), a);
}
