//! Aggregate counts share the approval read gate, never the profile directory.

use super::{
    APPROVER, SUBMITTER, a_pending_unit, body_json, decision_path, named_harness, with_headers,
};
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

const COUNTS: &str = "/bss-pricing/v1/approvals/counts";

#[tokio::test]
async fn counts_follow_decisions_without_profile_lookup() {
    for (action, state, actor, body) in [
        ("approve", "approved", APPROVER, None),
        (
            "reject",
            "rejected",
            APPROVER,
            Some(json!({"reason":"not accepted"})),
        ),
        ("withdraw", "voided", SUBMITTER, None),
    ] {
        let (h, source) = named_harness().await;
        let empty = h
            .allowed()
            .send(with_headers("GET", COUNTS, None, &[]))
            .await;
        assert_eq!(empty.status(), StatusCode::OK);
        assert_eq!(empty.headers()["cache-control"], "private, no-store");
        assert_eq!(
            body_json(empty).await,
            json!({"total":0,"submitted":0,"approved":0,"rejected":0,"voided":0})
        );
        let id = a_pending_unit(&h).await;
        let pending = body_json(
            h.allowed()
                .send(with_headers("GET", COUNTS, None, &[]))
                .await,
        )
        .await;
        assert_eq!(
            pending,
            json!({"total":1,"submitted":1,"approved":0,"rejected":0,"voided":0})
        );
        let result = h
            .allowed_as(actor)
            .send(with_headers("POST", &decision_path(id, action), body, &[]))
            .await;
        assert_eq!(
            result.status(),
            StatusCode::OK,
            "{}",
            body_json(result).await
        );
        let decided = body_json(
            h.allowed()
                .send(with_headers("GET", COUNTS, None, &[]))
                .await,
        )
        .await;
        assert_eq!(decided["total"], 1);
        assert_eq!(decided["submitted"], 0);
        assert_eq!(decided[state], 1);
        assert!(source.calls.lock().is_empty());
    }
}

#[tokio::test]
async fn counts_preserve_tenant_and_resource_scope_and_reject_query_parameters() {
    use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
    let (h, source) = named_harness().await;
    let id = a_pending_unit(&h).await;
    for (client, expected) in [
        (h.anonymous(), StatusCode::UNAUTHORIZED),
        (h.denied(), StatusCode::FORBIDDEN),
        (h.unavailable(), StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let response = client.send(with_headers("GET", COUNTS, None, &[])).await;
        assert_eq!(response.status(), expected);
    }
    let foreign = body_json(
        h.scope_mismatch()
            .send(with_headers("GET", COUNTS, None, &[]))
            .await,
    )
    .await;
    assert_eq!(foreign["total"], 0);
    for query in [
        "$filter=state%20eq%20'submitted'",
        "limit=1",
        "$top=1",
        "cursor=bad",
        "state=submitted",
    ] {
        let response = h
            .allowed()
            .send(with_headers("GET", &format!("{COUNTS}?{query}"), None, &[]))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    for (allowed, expected) in [(id, 1), (Uuid::now_v7(), 0)] {
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![h.tenant]),
            ScopeFilter::in_uuids(pep_properties::RESOURCE_ID, vec![allowed]),
        ]));
        let counts = h
            .governance
            .approvals
            .counts(&scope, h.tenant)
            .await
            .expect("scoped aggregate");
        assert_eq!(counts.total, expected);
        assert_eq!(counts.submitted, expected);
    }
    assert!(source.calls.lock().is_empty());
}
