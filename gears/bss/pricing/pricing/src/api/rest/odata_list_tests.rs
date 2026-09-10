//! Unit tests for the pricing `OData` list extra-key guard.

use std::collections::HashMap;

use super::reject_non_odata_list_params;

#[test]
fn extra_key_guard_allows_odata_and_pagination() {
    let mut q = HashMap::new();
    q.insert(
        "$filter".to_owned(),
        "lifecycle_state eq 'draft'".to_owned(),
    );
    q.insert("$orderby".to_owned(), "plan_id".to_owned());
    q.insert("limit".to_owned(), "100".to_owned());
    q.insert("cursor".to_owned(), "abc".to_owned());
    reject_non_odata_list_params(&q).expect("allowed keys must pass");
}

#[test]
fn extra_key_guard_rejects_lifecycle_state() {
    let mut q = HashMap::new();
    q.insert("lifecycle_state".to_owned(), "draft".to_owned());
    let err = reject_non_odata_list_params(&q).expect_err("named lifecycle_state must reject");
    let detail = format!("{err}");
    assert!(detail.contains("lifecycle_state"), "{detail}");
    assert!(detail.contains("$filter"), "{detail}");
}

#[test]
fn extra_key_guard_rejects_status() {
    let mut q = HashMap::new();
    q.insert("status".to_owned(), "OPEN".to_owned());
    let err = reject_non_odata_list_params(&q).expect_err("plain status must reject");
    assert!(format!("{err}").contains("status"));
}
