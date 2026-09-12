#![allow(clippy::expect_used)]

use gts::GtsTypeId;
use quota_enforcement_sdk::{METRIC_BASE_TYPE, SCOPE_USER};
use serde_json::{Value, json};

use super::CompiledContract;
use crate::test_support::{
    DRAFT7, LLM_TOKEN_REQUEST, METRIC_TOKENS, MIXIN_REQUEST, TYPED_MODEL_BASE, TYPED_REQUEST,
    llm_gateway_documents, mixin_request_document, mixin_type_document, resolve_documents,
    typed_request_document,
};

const MODEL_GPT: &str = "gts.cf.qetest.models.model.v1~cf.qetest.models.gpt.v1";
const MODEL_CLAUDE: &str = "gts.cf.qetest.models.model.v1~cf.qetest.models.claude.v1";

fn compiled(id: &str, documents: &[serde_json::Value]) -> CompiledContract {
    let registered = resolve_documents(documents)
        .into_iter()
        .find(|r| r.id.as_ref() == id)
        .expect("fixture resolves");
    CompiledContract::compile(registered.id.clone(), registered.schema).expect("compiles")
}

#[test]
fn the_token_request_contract_validates_the_envelope_not_the_inner_object() {
    let contract = compiled(LLM_TOKEN_REQUEST, &llm_gateway_documents());
    assert_eq!(contract.type_id().as_ref(), LLM_TOKEN_REQUEST);
    contract
        .validate(&json!({ "type": LLM_TOKEN_REQUEST, "metadata": { "region": "eu-west-1" } }))
        .expect("a conforming envelope");
    let missing = contract
        .validate(&json!({ "type": LLM_TOKEN_REQUEST, "metadata": {} }))
        .expect_err("region is required");
    assert!(missing.iter().any(|m| m.contains("region")), "{missing:?}");
    let bare = contract
        .validate(&json!({ "region": "eu-west-1" }))
        .expect_err("the inner object alone misses the base's required keys");
    assert!(!bare.is_empty());
    let extra = contract
        .validate(&json!({ "type": LLM_TOKEN_REQUEST, "metadata": { "region": "eu", "x": 1 } }))
        .expect_err("the owner closed the metadata object");
    assert!(!extra.is_empty());
}

#[test]
fn gts_formats_and_x_gts_ref_are_enforced_on_metadata_values() {
    let contract = compiled(TYPED_REQUEST, &[typed_request_document(METRIC_TOKENS)]);
    let envelope = |model: &str| json!({ "type": TYPED_REQUEST, "metadata": { "model": model } });

    contract
        .validate(&envelope(
            "gts.cf.qetest.models.model.v1~cf.qetest.models.gpt.v1",
        ))
        .expect("an instance under the declared base");
    let malformed = contract
        .validate(&envelope("not-an-id"))
        .expect_err("gts-instance-id format is asserted");
    assert!(!malformed.is_empty());
    let other_base = contract
        .validate(&envelope(METRIC_TOKENS))
        .expect_err("a well-formed id under another base fails x-gts-ref");
    assert!(
        other_base
            .iter()
            .any(|m| m.contains("gts.cf.qetest.models.model.v1~")),
        "{other_base:?}"
    );
    let bad_time = contract
        .validate(&json!({
            "type": TYPED_REQUEST,
            "metadata": { "model": "gts.cf.qetest.models.model.v1~cf.qetest.models.gpt.v1", "when": "yesterday" }
        }))
        .expect_err("date-time is asserted under Draft-07");
    assert!(!bad_time.is_empty());
}

#[test]
fn local_and_mixin_references_apply_after_resolution() {
    let contract = compiled(
        MIXIN_REQUEST,
        &[mixin_type_document(), mixin_request_document(METRIC_TOKENS)],
    );
    contract
        .validate(&json!({
            "type": MIXIN_REQUEST,
            "metadata": { "region": "eu", "when": "2026-09-08T10:00:00Z" }
        }))
        .expect("conforming");
    assert!(
        contract
            .validate(&json!({ "type": MIXIN_REQUEST, "metadata": { "region": "mars" } }))
            .is_err(),
        "the mixin's enum applies"
    );
    assert!(
        contract
            .validate(&json!({ "type": MIXIN_REQUEST, "metadata": { "region": "eu", "when": 3 } }))
            .is_err(),
        "the local definition applies"
    );
}

#[test]
fn a_schema_that_does_not_compile_is_reported() {
    let err = CompiledContract::compile(
        GtsTypeId::new(LLM_TOKEN_REQUEST),
        json!({ "$schema": crate::test_support::DRAFT7, "type": "object", "properties": 5 }),
    )
    .expect_err("properties must be an object");
    assert!(!err.is_empty());
}

/// A one-field contract whose `value` carries the schema under test.
fn contract_over(value_schema: &Value) -> CompiledContract {
    CompiledContract::compile(
        GtsTypeId::new(TYPED_REQUEST),
        json!({
            "$schema": DRAFT7,
            "type": "object",
            "properties": { "value": value_schema },
            "required": ["value"],
        }),
    )
    .expect("compiles")
}

fn check(contract: &CompiledContract, value: &Value) -> Result<(), Vec<String>> {
    contract.validate(&json!({ "value": value }))
}

#[test]
fn one_of_branches_without_gts_constraints_are_judged_by_their_own_schema() {
    let contract =
        contract_over(&json!({ "oneOf": [{ "type": "string" }, { "type": "integer" }] }));
    check(&contract, &json!("text")).expect("only the string branch matches");
    check(&contract, &json!(7)).expect("only the integer branch matches");
    check(&contract, &json!(true)).expect_err("no branch matches");
}

#[test]
fn one_of_branches_are_told_apart_by_their_x_gts_ref() {
    let contract = contract_over(&json!({ "oneOf": [
        { "type": "string", "format": "gts-instance-id", "x-gts-ref": TYPED_MODEL_BASE },
        { "type": "string", "format": "gts-instance-id", "x-gts-ref": METRIC_BASE_TYPE },
    ] }));
    check(&contract, &json!(MODEL_GPT)).expect("exactly the model branch");
    check(&contract, &json!(METRIC_TOKENS)).expect("exactly the metric branch");
    check(&contract, &json!(SCOPE_USER)).expect_err("an instance under neither base");
    check(&contract, &json!("plain")).expect_err("not a GTS id");
}

#[test]
fn any_of_falls_back_to_a_branch_without_gts_constraints() {
    let contract = contract_over(&json!({ "anyOf": [
        { "type": "string", "x-gts-ref": TYPED_MODEL_BASE },
        { "type": "string", "maxLength": 5 },
    ] }));
    check(&contract, &json!(MODEL_GPT)).expect("the GTS branch");
    check(&contract, &json!("short")).expect("the plain branch");
    check(&contract, &json!(METRIC_TOKENS)).expect_err("wrong base and too long");
}

#[test]
fn all_of_combines_x_gts_ref_with_ordinary_keywords() {
    let contract = contract_over(&json!({ "allOf": [
        { "type": "string", "x-gts-ref": TYPED_MODEL_BASE },
        { "pattern": "gpt" },
    ] }));
    check(&contract, &json!(MODEL_GPT)).expect("both hold");
    check(&contract, &json!(MODEL_CLAUDE)).expect_err("the pattern fails");
    let wrong_base = check(&contract, &json!(METRIC_TOKENS)).expect_err("x-gts-ref fails");
    assert!(
        wrong_base.iter().any(|m| m.contains(TYPED_MODEL_BASE)),
        "{wrong_base:?}"
    );
}

#[test]
fn the_x_gts_ref_pointer_form_resolves_against_the_root_schema() {
    let contract = CompiledContract::compile(
        GtsTypeId::new(TYPED_REQUEST),
        json!({
            "$schema": DRAFT7,
            "$id": format!("gts://{TYPED_MODEL_BASE}"),
            "type": "object",
            "properties": { "value": { "type": "string", "x-gts-ref": "/$id" } },
        }),
    )
    .expect("compiles");
    check(&contract, &json!(MODEL_GPT)).expect("under the schema's own id");
    check(&contract, &json!(METRIC_TOKENS)).expect_err("another base");
}

#[test]
fn x_gts_ref_constrains_strings_and_leaves_other_values_to_type() {
    let untyped = contract_over(&json!({ "x-gts-ref": TYPED_MODEL_BASE }));
    check(&untyped, &json!(3)).expect("no string to match, no type keyword to fail");
    check(&untyped, &json!(METRIC_BASE_TYPE)).expect_err("a string must match");
    let typed = contract_over(&json!({ "type": "string", "x-gts-ref": TYPED_MODEL_BASE }));
    check(&typed, &json!(3)).expect_err("type rejects the integer");
}

#[test]
fn a_malformed_x_gts_ref_fails_compilation() {
    for bad in [
        json!(5),
        json!("not.a.pattern"),
        json!("/nowhere"),
        json!("gts.cf.*.qe.*"),
    ] {
        let err = CompiledContract::compile(
            GtsTypeId::new(TYPED_REQUEST),
            json!({
                "$schema": DRAFT7,
                "type": "object",
                "properties": { "value": { "x-gts-ref": bad } },
            }),
        )
        .expect_err("the constraint is rejected at compile time");
        assert!(err.contains("x-gts-ref"), "{err}");
    }
}
