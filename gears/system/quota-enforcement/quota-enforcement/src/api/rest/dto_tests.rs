#![allow(clippy::expect_used)]

use quota_enforcement_sdk::{PeriodType, QuotaStatus};
use serde_json::{Value, json};
use toolkit_contract::QueryParams;
use uuid::Uuid;

use super::{CreateQuotaDto, ListQuotasQuery, UpdateQuotaDto};
use crate::domain::error::DomainError;
use crate::domain::quotas::{CreateQuotaRequest, ListQuotasRequest, Presence, UpdateQuotaRequest};
use crate::domain::tokens;
use crate::test_support::{LLM_USER_PROJECTION, METRIC_TOKENS};

fn create_body() -> Value {
    json!({
        "tenant_id": Uuid::from_u128(1),
        "subject": { "projection_type": LLM_USER_PROJECTION, "subject_id": "u1" },
        "metric": METRIC_TOKENS,
        "quota_type": "gts.cf.qe.quota.type.v1~cf.qe.quota.consumption.v1",
        "period": "gts.cf.qe.period.type.v1~cf.qe.period.month.v1",
        "enforcement_mode": "gts.cf.qe.enforcement.type.v1~cf.qe.enforcement.hard.v1",
        "cap": -1,
        "source": "gts.cf.qe.source.type.v1~cf.qe.source.operator.v1"
    })
}

#[test]
fn create_bodies_keep_period_presence_and_signed_caps() {
    let dto: CreateQuotaDto = serde_json::from_value(create_body()).expect("body");
    assert_eq!(dto.period, Presence::Value(PeriodType::Month));
    assert_eq!(dto.cap, Some(-1), "a negative cap reaches the domain");
    let request = CreateQuotaRequest::try_from(dto).expect("request");
    assert_eq!(
        request.subject.projection_type.as_ref(),
        LLM_USER_PROJECTION
    );
    assert_eq!(request.metadata, None);

    let mut body = create_body();
    body["period"] = Value::Null;
    let dto: CreateQuotaDto = serde_json::from_value(body).expect("null period");
    assert_eq!(dto.period, Presence::Null);
    let mut body = create_body();
    body.as_object_mut().expect("object").remove("period");
    let dto: CreateQuotaDto = serde_json::from_value(body).expect("absent period");
    assert_eq!(dto.period, Presence::Absent);

    let mut body = create_body();
    body["bogus"] = json!(1);
    assert!(
        serde_json::from_value::<CreateQuotaDto>(body).is_err(),
        "unknown keys"
    );
    let mut body = create_body();
    body["constraint_contract"] = json!({ "type_id": "x", "version": 1 });
    assert!(
        serde_json::from_value::<CreateQuotaDto>(body).is_err(),
        "the contract is never caller-supplied"
    );
    let mut body = create_body();
    body["enforcement_mode"] = json!("gts.cf.qe.enforcement.type.v1~cf.qe.enforcement.soft.v1");
    assert!(
        serde_json::from_value::<CreateQuotaDto>(body).is_err(),
        "the enforcement mode enum is closed"
    );
    let mut body = create_body();
    body["subject"]["projection_type"] = json!("not a type id");
    let dto: CreateQuotaDto = serde_json::from_value(body).expect("body");
    assert_eq!(
        CreateQuotaRequest::try_from(dto).expect_err("projection parses in the domain"),
        DomainError::InvalidArgument {
            field: "subject.projection_type",
            reason: tokens::PROJECTION_INVALID
        }
    );
}

#[test]
fn update_bodies_distinguish_absent_null_and_value_for_every_gated_field() {
    let dto: UpdateQuotaDto = serde_json::from_value(json!({})).expect("empty body");
    assert_eq!(dto, UpdateQuotaDto::default());
    let dto: UpdateQuotaDto = serde_json::from_value(json!({
        "metric": null,
        "quota_type": "gts.cf.qe.quota.type.v1~cf.qe.quota.rate.v1",
        "cap": null,
        "validity_window": null,
        "notification_thresholds": []
    }))
    .expect("body");
    assert_eq!(dto.metric, Presence::Null);
    assert!(matches!(dto.quota_type, Presence::Value(Value::String(_))));
    assert_eq!(dto.period, Presence::Absent);
    assert_eq!(dto.cap, Presence::Null);
    assert_eq!(dto.validity_window, Presence::Null);
    assert_eq!(dto.notification_thresholds, Some(Vec::new()));
    let request = UpdateQuotaRequest::from(dto);
    assert_eq!(request.cap, Presence::Null);
    assert_eq!(request.validity_window, Presence::Null);

    let dto: UpdateQuotaDto = serde_json::from_value(json!({
        "cap": 7,
        "validity_window": { "start": "2026-01-01T00:00:00Z" }
    }))
    .expect("body");
    assert_eq!(dto.cap, Presence::Value(7));
    assert!(matches!(dto.validity_window, Presence::Value(_)));
    assert!(
        serde_json::from_value::<UpdateQuotaDto>(json!({ "bogus": 1 })).is_err(),
        "unknown keys"
    );
}

#[test]
fn list_queries_declare_a_repeated_id_and_parse_into_the_domain_request() {
    let id_param = ListQuotasQuery::openapi_params()
        .iter()
        .find(|p| p.name == "id")
        .expect("id declared");
    assert!(id_param.array, "repeated keys collect into a list");
    assert!(!id_param.required);

    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let request = ListQuotasRequest::try_from(ListQuotasQuery {
        tenant_id: Some(Uuid::from_u128(9).to_string()),
        projection_type: Some(LLM_USER_PROJECTION.to_owned()),
        subject_id: Some("u1".to_owned()),
        metric: Some(METRIC_TOKENS.to_owned()),
        status: Some("deactivated".to_owned()),
        id: vec![a.to_string(), b.to_string()],
        limit: Some(5),
        cursor: Some("c".to_owned()),
    })
    .expect("request");
    assert_eq!(
        request.ids.iter().map(|q| q.as_uuid()).collect::<Vec<_>>(),
        vec![a, b]
    );
    assert_eq!(request.status, Some(QuotaStatus::Deactivated));
    assert_eq!(request.limit, Some(5));

    for (query, field, reason) in [
        (
            ListQuotasQuery {
                tenant_id: Some("nope".to_owned()),
                ..ListQuotasQuery::default()
            },
            "tenant_id",
            tokens::TENANT_ID_INVALID,
        ),
        (
            ListQuotasQuery {
                projection_type: Some("nope".to_owned()),
                ..ListQuotasQuery::default()
            },
            "projection_type",
            tokens::PROJECTION_INVALID,
        ),
        (
            ListQuotasQuery {
                status: Some("paused".to_owned()),
                ..ListQuotasQuery::default()
            },
            "status",
            tokens::STATUS_INVALID,
        ),
        (
            ListQuotasQuery {
                id: vec!["nope".to_owned()],
                ..ListQuotasQuery::default()
            },
            "id",
            tokens::QUOTA_ID_INVALID,
        ),
    ] {
        assert_eq!(
            ListQuotasRequest::try_from(query).expect_err(field),
            DomainError::InvalidArgument { field, reason }
        );
    }
}
