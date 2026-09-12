//! Unit tests for the OData list extra-key guard and seller-tenant extractor.

use std::collections::HashMap;

use toolkit_odata::parse_filter_string;
use uuid::Uuid;

use super::{
    list_seller_tenant, reject_non_odata_list_params, reject_non_odata_list_params_allowing,
};

const CALLER: Uuid = uuid::uuid!("11111111-1111-1111-1111-111111111111");
const OTHER: Uuid = uuid::uuid!("22222222-2222-2222-2222-222222222222");

fn filter(raw: &str) -> toolkit_odata::ast::Expr {
    parse_filter_string(raw)
        .expect("filter must parse")
        .as_expr()
        .clone()
}

#[test]
fn extra_key_guard_allows_odata_and_pagination() {
    let mut q = HashMap::new();
    q.insert(
        "$filter".to_owned(),
        "tenant_id eq 11111111-1111-1111-1111-111111111111".to_owned(),
    );
    q.insert("$orderby".to_owned(), "account_id".to_owned());
    q.insert("limit".to_owned(), "25".to_owned());
    q.insert("cursor".to_owned(), "abc".to_owned());
    reject_non_odata_list_params(&q).expect("allowed keys must pass");
}

#[test]
fn extra_key_guard_rejects_tenant_id() {
    let mut q = HashMap::new();
    q.insert("tenant_id".to_owned(), CALLER.to_string());
    let err = reject_non_odata_list_params(&q).expect_err("named tenant_id must reject");
    let detail = format!("{err}");
    assert!(detail.contains("tenant_id"), "{detail}");
    assert!(detail.contains("$filter"), "{detail}");
}

#[test]
fn extra_key_guard_rejects_status() {
    let mut q = HashMap::new();
    q.insert("status".to_owned(), "OPEN".to_owned());
    let err = reject_non_odata_list_params(&q).expect_err("plain status must reject");
    assert!(format!("{err}").contains("status"));
}

#[test]
fn extra_key_guard_allows_named_valuation() {
    let mut q = HashMap::new();
    q.insert("valuation".to_owned(), "functional".to_owned());
    reject_non_odata_list_params_allowing(&q, &["valuation"])
        .expect("balances valuation must pass");
}

#[test]
fn seller_omitted_is_caller() {
    assert_eq!(list_seller_tenant(None, CALLER).expect("omit"), CALLER);
}

#[test]
fn seller_from_eq_uuid() {
    let expr = filter(&format!("tenant_id eq {OTHER}"));
    assert_eq!(list_seller_tenant(Some(&expr), CALLER).expect("eq"), OTHER);
}

#[test]
fn seller_and_with_other_dim_keeps_one() {
    let expr = filter(&format!("tenant_id eq {OTHER} and account_class eq 'AR'"));
    assert_eq!(list_seller_tenant(Some(&expr), CALLER).expect("and"), OTHER);
}

#[test]
fn seller_or_two_tenants_is_400() {
    let expr = filter(&format!("tenant_id eq {CALLER} or tenant_id eq {OTHER}"));
    list_seller_tenant(Some(&expr), CALLER).expect_err("mixed or must reject");
}

#[test]
fn seller_same_uuid_twice_is_one() {
    let expr = filter(&format!("tenant_id eq {OTHER} and tenant_id eq {OTHER}"));
    assert_eq!(
        list_seller_tenant(Some(&expr), CALLER).expect("same twice"),
        OTHER
    );
}

#[test]
fn seller_not_eq_is_ignored() {
    let expr = filter(&format!("not tenant_id eq {OTHER}"));
    assert_eq!(
        list_seller_tenant(Some(&expr), CALLER).expect("not must not select"),
        CALLER
    );
}

#[test]
fn seller_reversed_eq_is_one() {
    let expr = filter(&format!("{OTHER} eq tenant_id"));
    assert_eq!(
        list_seller_tenant(Some(&expr), CALLER).expect("reversed"),
        OTHER
    );
}

#[test]
fn seller_string_uuid_literal_is_one() {
    let expr = filter(&format!("tenant_id eq '{OTHER}'"));
    assert_eq!(
        list_seller_tenant(Some(&expr), CALLER).expect("string uuid"),
        OTHER
    );
}

#[test]
fn seller_in_one_is_that_seller() {
    let expr = filter(&format!("tenant_id in ({OTHER})"));
    assert_eq!(
        list_seller_tenant(Some(&expr), CALLER).expect("in one"),
        OTHER
    );
}

#[test]
fn seller_in_two_is_400() {
    let expr = filter(&format!("tenant_id in ({CALLER},{OTHER})"));
    list_seller_tenant(Some(&expr), CALLER).expect_err("in two must reject");
}
