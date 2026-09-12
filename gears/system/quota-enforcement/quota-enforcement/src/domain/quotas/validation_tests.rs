#![allow(clippy::expect_used)]

use gts::GtsTypeId;
use quota_enforcement_sdk::{
    CapPatch, ContractRef, EnforcementMode, MetricId, PeriodType, Quota, QuotaId, QuotaPatch,
    QuotaSource, QuotaStatus, QuotaType, SubjectRef, SubjectScope, TenantId, ValidityWindow,
    ValidityWindowPatch,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    QuotaLimits, RATE_QUOTAS, SUBJECT_ID_MAX_LEN, validate_create_shape, validate_list,
    validate_patched_shape, validate_subject_scope, validate_update_shape,
};
use crate::domain::error::DomainError;
use crate::domain::quotas::request::{
    CreateQuotaRequest, ListQuotasRequest, Presence, UpdateQuotaRequest,
};
use crate::domain::tokens;
use crate::test_support::{LLM_USER_PROJECTION, METRIC_TOKENS, tenant};

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).expect("timestamp")
}

fn subject(id: &str) -> SubjectRef {
    SubjectRef {
        projection_type: GtsTypeId::try_new(LLM_USER_PROJECTION).expect("type"),
        subject_id: id.to_owned(),
    }
}

fn consumption() -> CreateQuotaRequest {
    CreateQuotaRequest {
        tenant_id: tenant(),
        subject: subject("u1"),
        metric: METRIC_TOKENS.to_owned(),
        quota_type: QuotaType::Consumption,
        period: Presence::Value(PeriodType::Month),
        enforcement_mode: EnforcementMode::Hard,
        cap: Some(100),
        notification_thresholds: vec![50, 90],
        validity_window: None,
        fail_open_hint: false,
        metadata: None,
        source: QuotaSource::Operator,
    }
}

fn allocation() -> CreateQuotaRequest {
    CreateQuotaRequest {
        quota_type: QuotaType::Allocation,
        period: Presence::Absent,
        ..consumption()
    }
}

fn invalid(field: &str, reason: &str) -> DomainError {
    let field: &'static str = Box::leak(field.to_owned().into_boxed_str());
    let reason: &'static str = Box::leak(reason.to_owned().into_boxed_str());
    DomainError::InvalidArgument { field, reason }
}

fn limits() -> QuotaLimits {
    QuotaLimits {
        metadata_max_bytes: 4096,
        list_max_limit: 500,
        list_max_ids: 100,
    }
}

// --- create ------------------------------------------------------------------

#[test]
fn a_valid_consumption_draft_passes_and_keeps_its_fields() {
    let draft = validate_create_shape(consumption()).expect("valid");
    assert_eq!(draft.metric.as_str(), METRIC_TOKENS);
    assert_eq!(draft.period, Some(PeriodType::Month));
    assert_eq!(draft.cap, Some(100));
    assert_eq!(draft.notification_thresholds, vec![50, 90]);
    assert!(
        draft.metadata.is_empty(),
        "absent metadata is an empty object"
    );
}

#[test]
fn rate_is_not_yet_implemented_even_when_other_rules_would_fail() {
    let err = validate_create_shape(CreateQuotaRequest {
        quota_type: QuotaType::Rate,
        cap: Some(-1),
        ..consumption()
    })
    .expect_err("rate");
    assert_eq!(
        err,
        DomainError::NotYetImplemented {
            feature: RATE_QUOTAS
        }
    );
}

#[test]
fn allocation_rejects_any_period_and_consumption_requires_one() {
    for present in [Presence::Null, Presence::Value(PeriodType::Day)] {
        let err = validate_create_shape(CreateQuotaRequest {
            period: present,
            ..allocation()
        })
        .expect_err("allocation with a period");
        assert_eq!(err, invalid("period", tokens::PERIOD_NOT_ALLOWED));
    }
    assert!(validate_create_shape(allocation()).is_ok());
    for absent in [Presence::Absent, Presence::Null] {
        let err = validate_create_shape(CreateQuotaRequest {
            period: absent,
            ..consumption()
        })
        .expect_err("consumption without a period");
        assert_eq!(err, invalid("period", tokens::PERIOD_REQUIRED));
    }
    let one_time = validate_create_shape(CreateQuotaRequest {
        period: Presence::Value(PeriodType::OneTime),
        ..consumption()
    })
    .expect("one_time is a valid period");
    assert_eq!(one_time.period, Some(PeriodType::OneTime));
}

#[test]
fn caps_are_non_negative_and_zero_or_unbounded_are_valid() {
    let err = validate_create_shape(CreateQuotaRequest {
        cap: Some(-1),
        ..consumption()
    })
    .expect_err("negative");
    assert_eq!(err, DomainError::CapMustBeNonNegative { cap: -1 });
    let zero = validate_create_shape(CreateQuotaRequest {
        cap: Some(0),
        ..consumption()
    })
    .expect("zero denies everything");
    assert_eq!(zero.cap, Some(0));
    let unbounded = validate_create_shape(CreateQuotaRequest {
        cap: None,
        notification_thresholds: Vec::new(),
        ..consumption()
    })
    .expect("unbounded");
    assert_eq!(unbounded.cap, None);
    let max = validate_create_shape(CreateQuotaRequest {
        cap: Some(i64::MAX),
        ..consumption()
    })
    .expect("the largest supported cap");
    assert_eq!(max.cap, Some(Quota::MAX_CAP));
}

#[test]
fn thresholds_are_in_range_ascending_and_need_a_bounded_cap() {
    for (thresholds, reason) in [
        (vec![0], tokens::THRESHOLD_OUT_OF_RANGE),
        (vec![101], tokens::THRESHOLD_OUT_OF_RANGE),
        (vec![50, 50], tokens::THRESHOLDS_NOT_ASCENDING),
        (vec![80, 50], tokens::THRESHOLDS_NOT_ASCENDING),
    ] {
        let err = validate_create_shape(CreateQuotaRequest {
            notification_thresholds: thresholds.clone(),
            ..consumption()
        })
        .expect_err("bad thresholds");
        assert_eq!(
            err,
            invalid("notification_thresholds", reason),
            "{thresholds:?}"
        );
    }
    let err = validate_create_shape(CreateQuotaRequest {
        cap: None,
        ..consumption()
    })
    .expect_err("thresholds on an unbounded cap");
    assert_eq!(err, DomainError::ThresholdsRequireBoundedCap);
}

#[test]
fn windows_must_not_be_inverted_and_ids_and_metrics_must_parse() {
    let err = validate_create_shape(CreateQuotaRequest {
        validity_window: Some(ValidityWindow {
            start: Some(ts(200)),
            end: Some(ts(100)),
        }),
        ..consumption()
    })
    .expect_err("inverted");
    assert_eq!(
        err,
        invalid("validity_window", tokens::VALIDITY_WINDOW_INVERTED)
    );
    let err = validate_create_shape(CreateQuotaRequest {
        subject: subject("  "),
        ..consumption()
    })
    .expect_err("blank subject id");
    assert_eq!(
        err,
        invalid("subject.subject_id", tokens::SUBJECT_ID_REQUIRED)
    );
    let err = validate_create_shape(CreateQuotaRequest {
        metric: "gts.cf.qe.metric.type.v1~".to_owned(),
        ..consumption()
    })
    .expect_err("a type id is not a metric");
    assert_eq!(err, invalid("metric", tokens::METRIC_INVALID));
}

// --- update ------------------------------------------------------------------

#[test]
fn a_patch_naming_rate_answers_unimplemented_before_the_immutable_gate() {
    let err = validate_update_shape(UpdateQuotaRequest {
        metric: Presence::Value(json!("x")),
        quota_type: Presence::Value(json!(QuotaType::Rate.as_gts_id())),
        ..UpdateQuotaRequest::default()
    })
    .expect_err("rate first");
    assert_eq!(
        err,
        DomainError::NotYetImplemented {
            feature: RATE_QUOTAS
        }
    );
}

#[test]
fn every_present_immutable_field_is_rejected_null_included() {
    let cases: Vec<(&str, UpdateQuotaRequest)> = vec![
        (
            "metric",
            UpdateQuotaRequest {
                metric: Presence::Null,
                ..UpdateQuotaRequest::default()
            },
        ),
        (
            "quota_type",
            UpdateQuotaRequest {
                quota_type: Presence::Value(json!(QuotaType::Allocation.as_gts_id())),
                ..UpdateQuotaRequest::default()
            },
        ),
        (
            "period",
            UpdateQuotaRequest {
                period: Presence::Value(Value::Null),
                cap: Presence::Value(5),
                ..UpdateQuotaRequest::default()
            },
        ),
        (
            "subject",
            UpdateQuotaRequest {
                subject: Presence::Value(json!({ "subject_id": "u2" })),
                ..UpdateQuotaRequest::default()
            },
        ),
    ];
    for (field, request) in cases {
        let err = validate_update_shape(request).expect_err(field);
        assert_eq!(err, invalid(field, tokens::IMMUTABLE_FIELD), "{field}");
    }
}

#[test]
fn the_patch_is_built_after_the_gates_and_may_not_be_empty() {
    let err = validate_update_shape(UpdateQuotaRequest::default()).expect_err("empty");
    assert_eq!(err, invalid("patch", tokens::PATCH_EMPTY));
    let patch = validate_update_shape(UpdateQuotaRequest {
        cap: Presence::Null,
        notification_thresholds: Some(Vec::new()),
        validity_window: Presence::Null,
        fail_open_hint: Some(true),
        ..UpdateQuotaRequest::default()
    })
    .expect("a consistent patch");
    assert_eq!(
        patch,
        QuotaPatch {
            cap: Some(CapPatch::Unbounded),
            notification_thresholds: Some(Vec::new()),
            validity_window: Some(ValidityWindowPatch::Clear),
            fail_open_hint: Some(true),
            ..QuotaPatch::default()
        }
    );
    let bounded = validate_update_shape(UpdateQuotaRequest {
        cap: Presence::Value(7),
        validity_window: Presence::Value(ValidityWindow {
            start: Some(ts(1)),
            end: None,
        }),
        ..UpdateQuotaRequest::default()
    })
    .expect("bounded");
    assert_eq!(bounded.cap, Some(CapPatch::Bounded(7)));
    assert!(matches!(
        bounded.validity_window,
        Some(ValidityWindowPatch::Set(_))
    ));
    let negative = validate_update_shape(UpdateQuotaRequest {
        cap: Presence::Value(-5),
        ..UpdateQuotaRequest::default()
    })
    .expect_err("negative cap");
    assert_eq!(negative, DomainError::CapMustBeNonNegative { cap: -5 });
    let unbinding_with_thresholds = validate_update_shape(UpdateQuotaRequest {
        cap: Presence::Null,
        notification_thresholds: Some(vec![50]),
        ..UpdateQuotaRequest::default()
    })
    .expect_err("thresholds next to an unbinding cap");
    assert_eq!(
        unbinding_with_thresholds,
        DomainError::ThresholdsRequireBoundedCap
    );
}

fn stored(cap: Option<u64>, thresholds: Vec<u8>) -> Quota {
    Quota {
        id: QuotaId::new(Uuid::from_u128(1)),
        tenant_id: TenantId::new(Uuid::from_u128(2)),
        subject: subject("u1"),
        metric: MetricId::parse(METRIC_TOKENS).expect("metric"),
        quota_type: QuotaType::Consumption,
        period: Some(PeriodType::Month),
        enforcement_mode: EnforcementMode::Hard,
        cap,
        notification_thresholds: thresholds,
        validity_window: None,
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
fn the_patched_shape_check_sees_the_merged_row() {
    let with_thresholds = stored(Some(100), vec![50]);
    let unbind = QuotaPatch {
        cap: Some(CapPatch::Unbounded),
        ..QuotaPatch::default()
    };
    assert_eq!(
        validate_patched_shape(&with_thresholds, &unbind),
        Err(DomainError::ThresholdsRequireBoundedCap)
    );
    let unbounded = stored(None, Vec::new());
    let add_thresholds = QuotaPatch {
        notification_thresholds: Some(vec![50]),
        ..QuotaPatch::default()
    };
    assert_eq!(
        validate_patched_shape(&unbounded, &add_thresholds),
        Err(DomainError::ThresholdsRequireBoundedCap)
    );
    let bound_and_add = QuotaPatch {
        cap: Some(CapPatch::Bounded(10)),
        notification_thresholds: Some(vec![50]),
        ..QuotaPatch::default()
    };
    assert_eq!(validate_patched_shape(&unbounded, &bound_and_add), Ok(()));
}

// --- subject scope and list ----------------------------------------------------

#[test]
fn the_subject_id_must_match_the_declared_scope() {
    let t = tenant();
    assert_eq!(
        validate_subject_scope(&SubjectScope::tenant(), t, &t.to_string()),
        Ok(())
    );
    assert_eq!(
        validate_subject_scope(&SubjectScope::tenant(), t, "someone-else"),
        Err(invalid(
            "subject.subject_id",
            tokens::SUBJECT_SCOPE_VIOLATION
        ))
    );
    assert_eq!(
        validate_subject_scope(&SubjectScope::user(), t, "u1"),
        Ok(())
    );
    assert_eq!(
        validate_subject_scope(&SubjectScope::user(), t, "   "),
        Err(invalid(
            "subject.subject_id",
            tokens::SUBJECT_SCOPE_VIOLATION
        ))
    );
    let too_long = "x".repeat(SUBJECT_ID_MAX_LEN + 1);
    assert_eq!(
        validate_subject_scope(&SubjectScope::user(), t, &too_long),
        Err(invalid(
            "subject.subject_id",
            tokens::SUBJECT_SCOPE_VIOLATION
        ))
    );
}

#[test]
fn list_requests_pair_the_subject_halves_and_respect_the_bounds() {
    let (filter, page) = validate_list(ListQuotasRequest::default(), &limits()).expect("defaults");
    assert_eq!(filter.subject, None);
    assert_eq!(page.limit, 100, "the platform default page");

    let err = validate_list(
        ListQuotasRequest {
            projection_type: Some(GtsTypeId::try_new(LLM_USER_PROJECTION).expect("type")),
            ..ListQuotasRequest::default()
        },
        &limits(),
    )
    .expect_err("half a subject");
    assert_eq!(err, invalid("subject_id", tokens::LIST_SUBJECT_INCOMPLETE));
    let err = validate_list(
        ListQuotasRequest {
            subject_id: Some("u1".to_owned()),
            ..ListQuotasRequest::default()
        },
        &limits(),
    )
    .expect_err("the other half");
    assert_eq!(
        err,
        invalid("projection_type", tokens::LIST_SUBJECT_INCOMPLETE)
    );

    let (filter, _) = validate_list(
        ListQuotasRequest {
            projection_type: Some(GtsTypeId::try_new(LLM_USER_PROJECTION).expect("type")),
            subject_id: Some("u1".to_owned()),
            metric: Some(METRIC_TOKENS.to_owned()),
            ..ListQuotasRequest::default()
        },
        &limits(),
    )
    .expect("both halves");
    assert_eq!(filter.subject, Some(subject("u1")));
    assert_eq!(
        filter.metric.map(|m| m.as_str().to_owned()),
        Some(METRIC_TOKENS.to_owned())
    );

    for (limit, ok) in [(0, false), (1, true), (500, true), (501, false)] {
        let outcome = validate_list(
            ListQuotasRequest {
                limit: Some(limit),
                ..ListQuotasRequest::default()
            },
            &limits(),
        );
        assert_eq!(outcome.is_ok(), ok, "limit {limit}");
        if !ok {
            assert_eq!(
                outcome.expect_err("bounded"),
                invalid("limit", tokens::LIST_LIMIT_OUT_OF_RANGE)
            );
        }
    }
    let too_many = ListQuotasRequest {
        ids: (0..101).map(|n| QuotaId::new(Uuid::from_u128(n))).collect(),
        ..ListQuotasRequest::default()
    };
    assert_eq!(
        validate_list(too_many, &limits()).expect_err("ids bound"),
        invalid("id", tokens::LIST_TOO_MANY_IDS)
    );
    let bad_metric = ListQuotasRequest {
        metric: Some("not-a-metric".to_owned()),
        ..ListQuotasRequest::default()
    };
    assert_eq!(
        validate_list(bad_metric, &limits()).expect_err("metric parses"),
        invalid("metric", tokens::METRIC_INVALID)
    );
}
