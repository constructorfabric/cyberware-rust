//! The synchronous checks of SPEC §8.1, exercised through [`super::validate`] —
//! which has no database in scope, so every refusal here is provably reachable
//! without touching entity state.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;

use serde_json::{Value, json};
use toolkit_gts::gts_id;

use super::super::{Candidate, Precondition, SubmitRequest};
use super::{AcceptanceContext, AcceptanceError, validate};
use crate::config::{PolicyEntry, TypesRegistryConfig};
use crate::domain::enums::OperationKind;
use crate::domain::policy::RegistrationPolicy;

fn noop_metrics() -> std::sync::Arc<dyn crate::domain::ports::metrics::AdmissionMetrics> {
    std::sync::Arc::new(crate::domain::ports::metrics::NoopMetrics)
}

const CF_TYPE: &str = gts_id!("cf.core.example.type.v1~");
const ACME_TYPE: &str = gts_id!("acme.crm.customer.type.v1~");

fn schema() -> Value {
    json!({
        "$id": format!("gts://{CF_TYPE}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    })
}

fn candidate(gts_id: &str) -> Candidate {
    Candidate {
        gts_id: gts_id.to_owned(),
        content: Some(schema()),
        expected_resource_version: None,
        force: false,
    }
}

fn request(candidates: Vec<Candidate>) -> SubmitRequest {
    SubmitRequest {
        idempotency_key: "key-1".to_owned(),
        kind: OperationKind::Registration,
        dry_run: false,
        candidates,
    }
}

/// A closed policy — the shipped default — plus default limits.
fn closed() -> (RegistrationPolicy, TypesRegistryConfig) {
    (
        RegistrationPolicy::default(),
        TypesRegistryConfig::default(),
    )
}

fn open_for_acme() -> (RegistrationPolicy, TypesRegistryConfig) {
    let mut map = BTreeMap::new();
    map.insert(
        gts_id!("acme.*").to_owned(),
        PolicyEntry {
            allowed_vendors: Some(vec!["acme".to_owned()]),
            tenant_ownable: None,
        },
    );
    (
        RegistrationPolicy::compile(&map).expect("compile"),
        TypesRegistryConfig::default(),
    )
}

fn run(
    pair: &(RegistrationPolicy, TypesRegistryConfig),
    request: &SubmitRequest,
) -> Result<super::Validated, AcceptanceError> {
    validate(
        &AcceptanceContext {
            policy: &pair.0,
            config: &pair.1,
            metrics: &noop_metrics(),
        },
        request,
    )
}

// ---------------------------------------------------------------------------
// The happy path, and what it records
// ---------------------------------------------------------------------------

/// A platform-vendor creation under the shipped defaults, and the item it
/// produces: precondition `0` for must-not-exist, and the canonical body as the
/// request payload (`ck_tr_operation_item_state` requires a payload while the item
/// is non-terminal).
#[test]
fn a_platform_vendor_creation_is_accepted_and_records_its_item() {
    let pair = closed();
    let validated = run(&pair, &request(vec![candidate(CF_TYPE)])).expect("accepted");

    assert_eq!(validated.items.len(), 1);
    let item = &validated.items[0];
    assert_eq!(item.item_no, 0);
    assert_eq!(item.gts_id, CF_TYPE);
    assert_eq!(item.precondition, Precondition::MustNotExist);
    assert!(
        item.request_payload.starts_with(r#"{"$id":"#),
        "canonical body"
    );
    assert_eq!(validated.request_fingerprint.as_bytes().len(), 32);
    assert_eq!(validated.idempotency_scope_hash.as_bytes().len(), 32);
}

/// Items are numbered in submission order, which is what the fingerprint hashes
/// and what the worker's outcome list reports against.
#[test]
fn items_are_numbered_in_submission_order() {
    let pair = closed();
    let validated = run(
        &pair,
        &request(vec![
            candidate(gts_id!("cf.core.b.type.v1~")),
            candidate(gts_id!("cf.core.a.type.v1~")),
        ]),
    )
    .expect("accepted");
    assert_eq!(validated.items[0].gts_id, gts_id!("cf.core.b.type.v1~"));
    assert_eq!(validated.items[0].item_no, 0);
    assert_eq!(validated.items[1].item_no, 1);
}

// ---------------------------------------------------------------------------
// Step 1: envelope
// ---------------------------------------------------------------------------

#[test]
fn a_missing_idempotency_key_is_refused_synchronously() {
    let pair = closed();
    for key in ["", "   "] {
        let mut req = request(vec![candidate(CF_TYPE)]);
        req.idempotency_key = key.to_owned();
        assert!(matches!(
            run(&pair, &req),
            Err(AcceptanceError::MissingIdempotencyKey)
        ));
    }
}

/// The column is `varchar(255)`, so an over-long key would fail as a database
/// error rather than as a refusal the caller can read.
#[test]
fn an_over_long_idempotency_key_is_refused_before_the_database_sees_it() {
    let pair = closed();
    let mut req = request(vec![candidate(CF_TYPE)]);
    req.idempotency_key = "k".repeat(256);
    assert!(matches!(
        run(&pair, &req),
        Err(AcceptanceError::IdempotencyKeyTooLong { length: 256 })
    ));
}

#[test]
fn an_empty_batch_is_refused() {
    let pair = closed();
    assert!(matches!(
        run(&pair, &request(Vec::new())),
        Err(AcceptanceError::EmptyBatch)
    ));
}

#[test]
fn a_batch_over_the_limit_is_refused_with_both_numbers() {
    let (policy, mut config) = closed();
    config.limits.batch_candidates = 2;
    let pair = (policy, config);
    let candidates = (0..3)
        .map(|i| {
            candidate(&format!(
                "{}cf.core.t{i}.type.v1~",
                toolkit_gts::GTS_ID_PREFIX
            ))
        })
        .collect();
    match run(&pair, &request(candidates)) {
        Err(AcceptanceError::BatchTooLarge { count, limit }) => {
            assert_eq!((count, limit), (3, 2));
        }
        other => panic!("expected BatchTooLarge, got {other:?}"),
    }
}

/// Deletion has its own protocol and its own precondition rule (T20). Refused
/// loudly rather than accepted into an operation whose rules do not exist yet.
#[test]
fn a_deletion_is_refused_until_t20() {
    let pair = closed();
    let mut req = request(vec![candidate(CF_TYPE)]);
    req.kind = OperationKind::Deletion;
    assert!(matches!(
        run(&pair, &req),
        Err(AcceptanceError::UnsupportedOperationKind)
    ));
}

/// Dry Run needs T20's rollback-only evaluation transaction. Until that path
/// exists it is refused before acceptance, so no operation can be stranded.
#[test]
fn a_dry_run_is_refused_until_t20() {
    let pair = closed();
    let mut req = request(vec![candidate(CF_TYPE)]);
    req.dry_run = true;
    assert!(matches!(
        run(&pair, &req),
        Err(AcceptanceError::DryRunNotAccepted)
    ));
}

// ---------------------------------------------------------------------------
// Step 2: identifiers
// ---------------------------------------------------------------------------

#[test]
fn an_unparsable_identifier_is_refused_with_the_library_reason() {
    let pair = closed();
    match run(&pair, &request(vec![candidate("gts.too.few~")])) {
        Err(AcceptanceError::InvalidIdentifier { gts_id, reason }) => {
            assert_eq!(gts_id, "gts.too.few~");
            assert!(!reason.is_empty());
        }
        other => panic!("expected InvalidIdentifier, got {other:?}"),
    }
}

/// A non-canonical spelling is refused rather than rewritten: two spellings of one
/// identifier in a batch would fingerprint differently while naming one entity.
#[test]
fn a_non_canonical_spelling_is_refused_rather_than_normalized() {
    let pair = closed();
    let padded = format!("  {CF_TYPE}  ");
    match run(&pair, &request(vec![candidate(&padded)])) {
        Err(AcceptanceError::InvalidIdentifier { reason, .. }) => {
            assert!(
                reason.contains(CF_TYPE),
                "the message must name the canonical form: {reason}"
            );
        }
        other => panic!("expected InvalidIdentifier, got {other:?}"),
    }
}

#[test]
fn a_duplicate_candidate_is_refused() {
    let pair = closed();
    match run(
        &pair,
        &request(vec![candidate(CF_TYPE), candidate(CF_TYPE)]),
    ) {
        Err(AcceptanceError::DuplicateCandidate { gts_id }) => assert_eq!(gts_id, CF_TYPE),
        other => panic!("expected DuplicateCandidate, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Step 3: registration policy, and the ordering invariant
// ---------------------------------------------------------------------------

#[test]
fn a_closed_region_refuses_a_declared_creation() {
    let pair = closed();
    match run(&pair, &request(vec![candidate(ACME_TYPE)])) {
        Err(AcceptanceError::PolicyRefused(inner)) => {
            assert_eq!(inner.0.parameter, "allowed_vendors");
            assert_eq!(inner.0.gts_id, ACME_TYPE);
        }
        other => panic!("expected PolicyRefused, got {other:?}"),
    }
}

/// SPEC §8.1 step 3: the gate is for **creations**. A revision names a version,
/// so closing a region must not freeze the entities already inside it.
///
/// Safe only because the declared kind is enforced downstream:
/// `unit::commit_revision` refuses an identifier the registry does not hold, so
/// naming a version cannot register anything new here.
#[test]
fn a_revision_bypasses_the_policy_gate_in_a_closed_region() {
    let pair = closed();
    let mut req = request(vec![candidate(ACME_TYPE)]);
    req.candidates[0].expected_resource_version = Some(4);
    let validated = run(&pair, &req).expect("a revision is not gated by the policy");
    assert_eq!(
        validated.items[0].precondition,
        Precondition::Version(4),
        "the precondition travels to the worker, which is what enforces the claim",
    );
}

/// The other side of the bypass: it is keyed on the precondition, not on the
/// region, so a creation in a region the policy *admits* still goes through the gate
/// and still passes it. The refusal half is
/// `a_closed_region_refuses_a_declared_creation` above.
#[test]
fn the_gate_admits_a_creation_in_an_opened_region() {
    let open = open_for_acme();
    let creation = request(vec![candidate(ACME_TYPE)]);
    let validated = run(&open, &creation).expect("an opened region admits its vendor");
    assert_eq!(
        validated.items[0].precondition,
        Precondition::MustNotExist,
        "and it is a creation that passed the gate, not a revision that skipped it",
    );
}

/// The ordering invariant, made observable: a candidate that fails **both** the
/// policy gate and the dialect gate is refused by the policy. Nothing here reads
/// entity state at all — `validate` has no database — so the invariant that a
/// refusal cannot probe the namespace holds structurally; what this pins is that
/// a later check cannot report first and leak which region exists.
#[test]
fn the_policy_gate_is_reported_before_the_later_checks() {
    let pair = closed();
    let mut req = request(vec![candidate(ACME_TYPE)]);
    req.candidates[0].content = Some(json!({ "type": "object" })); // no $schema either
    assert!(matches!(
        run(&pair, &req),
        Err(AcceptanceError::PolicyRefused(_))
    ));
}

#[test]
fn an_opened_region_admits_its_vendor() {
    let pair = open_for_acme();
    let mut req = request(vec![candidate(ACME_TYPE)]);
    req.candidates[0].content = Some(json!({
        "$id": format!("gts://{ACME_TYPE}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    }));
    run(&pair, &req).expect("an opened region admits its vendor");
}

// ---------------------------------------------------------------------------
// Step 4: identifier profile
// ---------------------------------------------------------------------------

#[test]
fn an_explicit_uuid_tail_is_refused() {
    let pair = closed();
    let with_tail = format!("{CF_TYPE}550e8400-e29b-41d4-a716-446655440000");
    match run(&pair, &request(vec![candidate(&with_tail)])) {
        Err(AcceptanceError::ExplicitUuidTail { gts_id }) => assert_eq!(gts_id, with_tail),
        other => panic!("expected ExplicitUuidTail, got {other:?}"),
    }
}

/// A registered Instance's last segment must name a stable major and carry no
/// minor. Both halves, plus the contrast that makes the rule meaningful: the same
/// shapes on a **Type Schema** identifier are admissible.
#[test]
fn an_instance_identifier_must_name_a_stable_major_without_a_minor() {
    let pair = closed();

    // A GTS Instance identifier is chained: a type segment, then the instance's
    // own segment with no trailing `~`. Two rules from `gts-rust` shape these
    // fixtures rather than this gate: a single-segment instance identifier does
    // not parse at all, and every segment is a full
    // `vendor.package.namespace.type.vMAJOR` — the same trap T4 recorded.
    for (id, fragment) in [
        (
            gts_id!("cf.core.example.type.v1~cf.crm.ns.thing.v0"),
            "major 0",
        ),
        (
            gts_id!("cf.core.example.type.v1~cf.crm.ns.thing.v1.2"),
            "minor",
        ),
    ] {
        match run(&pair, &request(vec![candidate(id)])) {
            Err(AcceptanceError::InstanceVersionProfile { gts_id, reason }) => {
                assert_eq!(gts_id, id);
                assert!(reason.contains(fragment), "{id}: {reason}");
            }
            other => panic!("expected InstanceVersionProfile for {id}, got {other:?}"),
        }
    }

    // The same shapes as Type Schemas: a minor is admissible under any prefix,
    // and a major-0 Type Schema is quarantined by references (T18), not by the
    // profile.
    for id in [
        gts_id!("cf.core.example.type.v1.2~"),
        gts_id!("cf.core.example.type.v0~"),
    ] {
        let mut req = request(vec![candidate(id)]);
        req.candidates[0].content = Some(json!({
            "$id": format!("gts://{id}"),
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
        }));
        run(&pair, &req).unwrap_or_else(|e| panic!("{id} must be admissible: {e}"));
    }
}

// ---------------------------------------------------------------------------
// Step 5: dialect
// ---------------------------------------------------------------------------

#[test]
fn a_type_schema_without_a_top_level_dialect_is_refused() {
    let pair = closed();
    let mut req = request(vec![candidate(CF_TYPE)]);
    req.candidates[0].content = Some(json!({ "type": "object" }));
    assert!(matches!(
        run(&pair, &req),
        Err(AcceptanceError::MissingDialect { .. })
    ));
}

/// The closed spelling set, and one outside it. Every accepted form normalizes
/// onto the canonical one, which is why a nested `…/schema` beside a root
/// `…/schema#` is not a conflict.
#[test]
fn the_dialect_spelling_set_is_closed_and_normalizing() {
    let pair = closed();
    for accepted in [
        "http://json-schema.org/draft-07/schema#",
        "http://json-schema.org/draft-07/schema",
        "https://json-schema.org/draft-07/schema#",
        "https://json-schema.org/draft-07/schema",
    ] {
        let mut req = request(vec![candidate(CF_TYPE)]);
        req.candidates[0].content = Some(json!({ "$schema": accepted, "type": "object" }));
        run(&pair, &req).unwrap_or_else(|e| panic!("{accepted} must be accepted: {e}"));
    }

    let mut req = request(vec![candidate(CF_TYPE)]);
    req.candidates[0].content = Some(
        json!({ "$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object" }),
    );
    match run(&pair, &req) {
        Err(AcceptanceError::UnsupportedDialect { found, .. }) => {
            assert!(found.contains("2020-12"));
        }
        other => panic!("expected UnsupportedDialect, got {other:?}"),
    }
}

/// A differing `$schema` below the root is refused with its path, and an
/// equivalent spelling below the root is not (ADR-0014).
#[test]
fn a_nested_dialect_must_not_differ_but_may_be_spelled_differently() {
    let pair = closed();

    let mut req = request(vec![candidate(CF_TYPE)]);
    req.candidates[0].content = Some(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "properties": { "inner": { "$schema": "https://json-schema.org/draft/2020-12/schema" } },
    }));
    match run(&pair, &req) {
        Err(AcceptanceError::ConflictingDialect { path, .. }) => {
            assert_eq!(path, "$.properties.inner");
        }
        other => panic!("expected ConflictingDialect, got {other:?}"),
    }

    let mut req = request(vec![candidate(CF_TYPE)]);
    req.candidates[0].content = Some(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "properties": { "inner": { "$schema": "https://json-schema.org/draft-07/schema" } },
    }));
    run(&pair, &req).expect("an equivalent nested spelling is not a conflict");
}

#[test]
fn a_nested_dialect_must_be_a_supported_string() {
    let pair = closed();

    for declared in [Value::Null, json!(7), json!({})] {
        let mut req = request(vec![candidate(CF_TYPE)]);
        req.candidates[0].content = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "properties": { "inner": { "$schema": declared } },
        }));
        match run(&pair, &req) {
            Err(AcceptanceError::ConflictingDialect { path, .. }) => {
                assert_eq!(path, "$.properties.inner");
            }
            other => panic!("expected ConflictingDialect, got {other:?}"),
        }
    }
}

/// The walk descends through array elements and more than one level of nesting,
/// and names the offender by its indexed path.
#[test]
fn a_nested_dialect_is_found_through_arrays_and_at_depth() {
    let pair = closed();

    let mut req = request(vec![candidate(CF_TYPE)]);
    req.candidates[0].content = Some(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "anyOf": [
            { "type": "object" },
            {
                "properties": {
                    "inner": { "$schema": "https://json-schema.org/draft/2020-12/schema" },
                },
            },
        ],
    }));
    match run(&pair, &req) {
        Err(AcceptanceError::ConflictingDialect { path, .. }) => {
            assert_eq!(path, "$.anyOf[1].properties.inner");
        }
        other => panic!("expected ConflictingDialect, got {other:?}"),
    }
}

#[test]
fn a_registration_without_a_document_is_refused() {
    let pair = closed();
    let mut req = request(vec![candidate(CF_TYPE)]);
    req.candidates[0].content = None;
    assert!(matches!(
        run(&pair, &req),
        Err(AcceptanceError::MissingContent { .. })
    ));
}

/// The authored-document limit is enforced on the **canonical** bytes, which is
/// what gets stored and fingerprinted.
#[test]
fn an_oversized_document_is_refused_against_the_configured_limit() {
    let (policy, mut config) = closed();
    config.limits.authored_document = crate::config::ByteSize::from_bytes(128);
    let pair = (policy, config);
    let mut req = request(vec![candidate(CF_TYPE)]);
    req.candidates[0].content = Some(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "description": "x".repeat(200),
    }));
    match run(&pair, &req) {
        Err(AcceptanceError::AuthoredDocumentTooLarge { size, limit, .. }) => {
            assert_eq!(limit, 128);
            assert!(size > 128);
        }
        other => panic!("expected AuthoredDocumentTooLarge, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Step 6: force
// ---------------------------------------------------------------------------

#[test]
fn force_is_refused_while_the_deployment_disallows_it() {
    let pair = closed();
    let mut req = request(vec![candidate(gts_id!("cf.core.example.type.v1.2~"))]);
    req.candidates[0].force = true;
    req.candidates[0].content = Some(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    }));
    assert!(matches!(
        run(&pair, &req),
        Err(AcceptanceError::ForceNotPermitted { .. })
    ));
}

/// With `force` permitted, the candidate must still *have* a cross-minor check to
/// waive. No-op shapes keep their precise refusal; a real case stays unavailable
/// until T17 can both evaluate it and persist its provenance.
#[test]
fn force_needs_a_cross_minor_check_to_waive() {
    let (policy, mut config) = closed();
    config.allow_compatibility_force = true;
    let pair = (policy, config);

    for nothing_to_waive in [
        gts_id!("cf.core.example.type.v1~"),   // major-only
        gts_id!("cf.core.example.type.v1.0~"), // first minor of its major
        gts_id!("cf.core.example.type.v0.3~"), // major 0
    ] {
        let mut req = request(vec![candidate(nothing_to_waive)]);
        req.candidates[0].force = true;
        req.candidates[0].content = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
        }));
        match run(&pair, &req) {
            Err(AcceptanceError::ForceHasNothingToWaive { gts_id }) => {
                assert_eq!(gts_id, nothing_to_waive);
            }
            other => {
                panic!("expected ForceHasNothingToWaive for {nothing_to_waive}, got {other:?}")
            }
        }
    }

    let mut req = request(vec![candidate(gts_id!("cf.core.example.type.v2.1~"))]);
    req.candidates[0].force = true;
    req.candidates[0].content = Some(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    }));
    assert!(matches!(
        run(&pair, &req),
        Err(AcceptanceError::ForceCompatibilityUnavailable { .. })
    ));
}

// ---------------------------------------------------------------------------
// Preconditions
// ---------------------------------------------------------------------------

/// A literal `0` is refused: the wire vocabulary spells must-not-exist as an
/// absent field, so a `0` is more likely a serialization accident than an intent.
#[test]
fn a_literal_zero_precondition_is_refused_while_absence_means_must_not_exist() {
    let pair = closed();
    let mut req = request(vec![candidate(CF_TYPE)]);
    req.candidates[0].expected_resource_version = Some(0);
    match run(&pair, &req) {
        Err(AcceptanceError::ZeroPrecondition { gts_id }) => assert_eq!(gts_id, CF_TYPE),
        other => panic!("expected ZeroPrecondition, got {other:?}"),
    }

    let validated = run(&pair, &request(vec![candidate(CF_TYPE)])).expect("absent is accepted");
    assert_eq!(validated.items[0].precondition, Precondition::MustNotExist);
}

#[test]
fn a_negative_precondition_is_refused() {
    let pair = closed();
    let mut req = request(vec![candidate(CF_TYPE)]);
    req.candidates[0].expected_resource_version = Some(-1);
    assert!(matches!(
        run(&pair, &req),
        Err(AcceptanceError::NegativePrecondition { version: -1, .. })
    ));
}

#[test]
fn a_minor_bearing_type_schema_cannot_be_content_revised() {
    let pair = closed();
    let id = gts_id!("cf.core.example.type.v1.2~");
    let mut req = request(vec![candidate(id)]);
    req.candidates[0].expected_resource_version = Some(1);
    match run(&pair, &req) {
        Err(AcceptanceError::MinorTypeSchemaRevision { gts_id }) => assert_eq!(gts_id, id),
        other => panic!("expected MinorTypeSchemaRevision, got {other:?}"),
    }
}
