use std::collections::BTreeMap;

use gts::GtsTypeId;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    ActiveQuotaCounts, CapPatch, ContractRef, Decision, DecisionResult, EnforcementMode,
    EvaluationAttribution, IdempotencySubjectKey, LeaseState, MetricId, MetricKind,
    NotificationEventKind, OperationType, PageRequest, PageResult, PeriodType, PolicyId,
    PolicyScope, ProjectionBinding, Quota, QuotaDebitPlan, QuotaDraft, QuotaId, QuotaPatch,
    QuotaSource, QuotaSpec, QuotaStatus, QuotaType, QuotaView, ResourceProjection, ScopeError,
    SubjectRef, SubjectScope, TenantId, UnknownValue, ValidityWindow,
};
use crate::gts::{SCOPE_TENANT, SCOPE_TYPE, SCOPE_USER};

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

// --- Scopes and attribution (projection-contracts feature) -----------------

#[test]
fn subject_scope_parse_and_deserialization_accept_only_scope_instances() {
    let user = SubjectScope::parse(SCOPE_USER).expect("user scope");
    assert_eq!(user, SubjectScope::user());
    assert!(!user.is_tenant());
    let tenant = SubjectScope::parse(SCOPE_TENANT).expect("tenant scope");
    assert_eq!(tenant, SubjectScope::tenant());
    assert!(tenant.is_tenant());

    let rejected = [
        SCOPE_TYPE,                                             // a type, not an instance
        "gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~", // another type
        "gts.cf.qe.metric.type.v1~cf.qe.metric.ai_requests.v1", // an instance of another type
        "not-a-gts-id",
        "",
    ];
    for raw in rejected {
        let direct = SubjectScope::parse(raw).expect_err(raw);
        assert!(
            matches!(
                direct,
                ScopeError::Invalid { .. } | ScopeError::NotAScope { .. }
            ),
            "{raw}: {direct:?}"
        );
        let json = serde_json::to_string(raw).expect("json string");
        assert!(
            serde_json::from_str::<SubjectScope>(&json).is_err(),
            "deserialization must run the same check: {raw}"
        );
    }
    assert!(matches!(
        SubjectScope::parse("gts.cf.qe.metric.type.v1~cf.qe.metric.ai_requests.v1"),
        Err(ScopeError::NotAScope { .. })
    ));
}

#[test]
fn subject_scope_serializes_as_its_instance_id_and_round_trips() {
    let json = serde_json::to_string(&SubjectScope::user()).expect("serialize");
    assert_eq!(json, format!("\"{SCOPE_USER}\""));
    let back: SubjectScope = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, SubjectScope::user());
    assert_eq!(back.as_gts().as_ref(), SCOPE_USER);
    assert_eq!(back.to_string(), SCOPE_USER);
}

#[test]
fn evaluation_attribution_distinguishes_omitted_metadata_from_null() {
    let tenant = Uuid::from_u128(7);
    let metric = "gts.cf.qe.metric.type.v1~cf.qe.metric.ai_requests.v1";

    let omitted: EvaluationAttribution =
        serde_json::from_value(json!({ "tenant_id": tenant, "metric": metric }))
            .expect("metadata may be omitted at the type level");
    assert_eq!(
        omitted.metadata, None,
        "the gear rejects the omission later"
    );
    assert!(omitted.subjects.is_empty());
    assert_eq!(omitted.resource, None);

    let empty: EvaluationAttribution =
        serde_json::from_value(json!({ "tenant_id": tenant, "metric": metric, "metadata": {} }))
            .expect("an empty object is a present object");
    assert_eq!(empty.metadata.as_ref().map(serde_json::Map::len), Some(0));

    for field in ["metadata", "resource"] {
        let explicit_null = json!({ "tenant_id": tenant, "metric": metric, field: null });
        assert!(
            serde_json::from_value::<EvaluationAttribution>(explicit_null).is_err(),
            "{field}: null is not absence"
        );
    }
    let unknown =
        json!({ "tenant_id": tenant, "metric": metric, "metadata": {}, "caller_type": "x" });
    assert!(serde_json::from_value::<EvaluationAttribution>(unknown).is_err());
}

#[test]
fn resource_projection_keeps_an_absent_id_absent() {
    let without_id: ResourceProjection = serde_json::from_value(json!({
        "type": "gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~",
        "metadata": { "model_family": "gpt" }
    }))
    .expect("id may be omitted");
    assert_eq!(without_id.id, None);
    let written = serde_json::to_value(&without_id).expect("serialize");
    let mut keys: Vec<String> = written
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["metadata", "type"],
        "no id key is written for an absent id"
    );

    let with_id: ResourceProjection = serde_json::from_value(json!({
        "type": "gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~",
        "id": "model-7",
        "metadata": {}
    }))
    .expect("id may be present");
    assert_eq!(with_id.id.as_deref(), Some("model-7"));

    for field in ["id", "metadata"] {
        let explicit_null = json!({
            "type": "gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~",
            "metadata": {},
            field: null
        });
        assert!(
            serde_json::from_value::<ResourceProjection>(explicit_null).is_err(),
            "{field}: null is rejected, the resource base requires a value when present"
        );
    }
    let no_metadata = json!({ "type": "gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~" });
    let parsed: ResourceProjection = serde_json::from_value(no_metadata).expect("type only");
    assert_eq!(
        parsed.metadata, None,
        "the gear rejects the omission at ingress"
    );
}

#[test]
fn contract_ref_for_type_takes_the_major_version_of_the_last_segment() {
    let id = GtsTypeId::try_new(
        "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v3~",
    )
    .expect("type id");
    let reference = ContractRef::for_type(&id).expect("versioned");
    assert_eq!(reference.type_id, id);
    assert_eq!(reference.version, 3);
}

#[test]
fn projection_bindings_are_distinct_by_pair() {
    let metric =
        MetricId::parse("gts.cf.qe.metric.type.v1~cf.qe.metric.ai_requests.v1").expect("metric");
    let user =
        GtsTypeId::try_new("gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~").expect("type");
    let tenant =
        GtsTypeId::try_new("gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.tenant.v1~").expect("type");
    let set: std::collections::HashSet<ProjectionBinding> = [
        ProjectionBinding {
            metric: metric.clone(),
            projection_type: user.clone(),
        },
        ProjectionBinding {
            metric: metric.clone(),
            projection_type: user,
        },
        ProjectionBinding {
            metric,
            projection_type: tenant,
        },
    ]
    .into_iter()
    .collect();
    assert_eq!(set.len(), 2);
}

// --- quota-lifecycle read and create shapes ---------------------------------

fn spec() -> QuotaSpec {
    QuotaSpec {
        tenant_id: TenantId::new(Uuid::from_u128(7)),
        subject: SubjectRef {
            projection_type: GtsTypeId::new("gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~"),
            subject_id: "u1".to_owned(),
        },
        metric: MetricId::parse("gts.cf.qe.metric.type.v1~cf.qe.metric.ai_tokens_input.v1")
            .expect("metric"),
        quota_type: QuotaType::Consumption,
        period: Some(PeriodType::Month),
        enforcement_mode: EnforcementMode::Hard,
        cap: Some(10),
        notification_thresholds: vec![50, 90],
        validity_window: None,
        fail_open_hint: false,
        metadata: serde_json::Map::new(),
        source: QuotaSource::Operator,
    }
}

fn contract() -> ContractRef {
    ContractRef {
        type_id: GtsTypeId::new(
            "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v1~",
        ),
        version: 1,
    }
}

fn stored(spec: QuotaSpec, window: Option<ValidityWindow>) -> Quota {
    let draft = QuotaDraft::from_spec(spec, contract());
    Quota {
        id: QuotaId::new(Uuid::from_u128(1)),
        tenant_id: draft.tenant_id,
        subject: draft.subject,
        metric: draft.metric,
        quota_type: draft.quota_type,
        period: draft.period,
        enforcement_mode: draft.enforcement_mode,
        cap: draft.cap,
        notification_thresholds: draft.notification_thresholds,
        validity_window: window,
        fail_open_hint: draft.fail_open_hint,
        metadata: draft.metadata,
        source: draft.source,
        status: QuotaStatus::Active,
        constraint_contract: draft.constraint_contract,
        record_version: 1,
        created_at: ts(0),
        updated_at: ts(0),
    }
}

#[test]
fn quota_view_is_computed_for_one_instant_and_flattens_the_record() {
    let window = ValidityWindow {
        start: Some(ts(100)),
        end: Some(ts(200)),
    };
    let quota = stored(spec(), Some(window));
    let inside = QuotaView::compute(quota.clone(), Some(MetricKind::Counter), ts(150));
    assert!(inside.currently_within_window);
    assert_eq!(inside.metric_kind, Some(MetricKind::Counter));
    let after = QuotaView::compute(quota.clone(), None, ts(201));
    assert!(
        !after.currently_within_window,
        "past the end, status untouched"
    );
    assert_eq!(after.quota.status, QuotaStatus::Active);
    let unbounded = QuotaView::compute(stored(spec(), None), Some(MetricKind::Gauge), ts(0));
    assert!(unbounded.currently_within_window);

    let value = serde_json::to_value(&inside).expect("serialize");
    assert_eq!(
        value["id"],
        json!(quota.id.to_string()),
        "record fields are flattened"
    );
    assert_eq!(value["currently_within_window"], json!(true));
    assert_eq!(value["metric_kind"], json!("counter"));
    let back: QuotaView = serde_json::from_value(value).expect("round trip");
    assert_eq!(back, inside);
}

#[test]
fn metric_kind_is_closed_and_snake_case() {
    assert_eq!(
        serde_json::to_value(MetricKind::Gauge).expect("serialize"),
        json!("gauge")
    );
    assert_eq!(
        serde_json::from_value::<MetricKind>(json!("counter")).expect("counter"),
        MetricKind::Counter
    );
    assert!(serde_json::from_value::<MetricKind>(json!("histogram")).is_err());
}

#[test]
fn quota_spec_has_no_contract_field_and_becomes_a_draft_with_one() {
    let spec = spec();
    let value = serde_json::to_value(&spec).expect("serialize");
    assert!(value.get("constraint_contract").is_none());
    let mut with_contract = value;
    with_contract["constraint_contract"] = json!({ "type_id": "x", "version": 1 });
    assert!(
        serde_json::from_value::<QuotaSpec>(with_contract).is_err(),
        "a caller cannot smuggle the contract in"
    );
    let draft = QuotaDraft::from_spec(spec.clone(), contract());
    assert_eq!(draft.constraint_contract, contract());
    assert_eq!(draft.metric, spec.metric);
    assert_eq!(draft.notification_thresholds, vec![50, 90]);
}

#[test]
fn max_cap_is_the_largest_signed_64_bit_value() {
    assert_eq!(Quota::MAX_CAP, 9_223_372_036_854_775_807);
    assert_eq!(i64::try_from(Quota::MAX_CAP), Ok(i64::MAX));
}

#[test]
fn active_quota_counts_default_to_zero_and_round_trip() {
    let mut counts = ActiveQuotaCounts::default();
    assert_eq!((counts.cap_zero, counts.cap_unbounded), (0, 0));
    counts.cap_zero = 2;
    counts.by_metric.insert(
        MetricId::parse("gts.cf.qe.metric.type.v1~cf.qe.metric.ai_tokens_input.v1")
            .expect("metric"),
        3,
    );
    let value = serde_json::to_value(&counts).expect("serialize");
    let back: ActiveQuotaCounts = serde_json::from_value(value).expect("round trip");
    assert_eq!(back, counts);
}

#[test]
fn notification_event_kind_names_equal_their_serialized_form() {
    for kind in [
        NotificationEventKind::ThresholdCrossed,
        NotificationEventKind::PeriodRollover,
        NotificationEventKind::LeaseAutoReleased,
        NotificationEventKind::LeaseResolvedByDeactivation,
        NotificationEventKind::QuotaChanged,
        NotificationEventKind::QuotaCounterAdjusted,
        NotificationEventKind::QuotaRollbackApplied,
        NotificationEventKind::PolicyChanged,
    ] {
        assert_eq!(
            serde_json::to_value(kind).expect("serialize"),
            json!(kind.as_str())
        );
    }
}
