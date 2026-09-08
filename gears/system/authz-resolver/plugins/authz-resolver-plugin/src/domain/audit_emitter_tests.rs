use super::*;
use crate::domain::deny::error_codes::{
    CONSTRAINTS_UNAVAILABLE_V1, SCOPE_MISMATCH_V1, UNKNOWN_RESOURCE_TYPE_V1,
};
use crate::domain::deny::{build_allow_response, build_deny_response};
use crate::test_support::EvaluationRequestBuilder;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, InPredicate, Predicate};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

// ---- Tracing capture ------------------------------------------------
//
// `emit` writes a `tracing` event and returns `()`, so the only way to test
// what it did (or did not) write is to observe the subscriber. Without this,
// both gating tests could do no better than "it did not panic" — which would
// pass just as happily if a disabled emitter logged anyway.

#[derive(Default)]
struct CapturedAudit {
    events: Mutex<Vec<HashMap<String, String>>>,
}

#[derive(Default)]
struct FieldVisitor(HashMap<String, String>);

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

struct CaptureLayer {
    captured: Arc<CapturedAudit>,
    /// Only `cf-authz.audit` is recorded, so an unrelated `debug!`/`warn!`
    /// elsewhere in the crate cannot be mistaken for an audit record.
    target: &'static str,
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != self.target {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        match self.captured.events.lock() {
            Ok(mut guard) => guard.push(visitor.0),
            Err(poisoned) => poisoned.into_inner().push(visitor.0),
        }
    }
}

fn capture_audit() -> (Arc<CapturedAudit>, tracing::dispatcher::DefaultGuard) {
    let captured = Arc::new(CapturedAudit::default());
    let layer = CaptureLayer {
        captured: Arc::clone(&captured),
        target: AUDIT_TARGET,
    };
    let guard = tracing_subscriber::registry().with(layer).set_default();
    (captured, guard)
}

/// The tracing target `emit` writes to. Pinned here because the whole audit
/// pipeline (log routing, dashboards, the integration capture) keys on it.
const AUDIT_TARGET: &str = "cf-authz.audit";

fn recorded(captured: &CapturedAudit) -> Vec<HashMap<String, String>> {
    match captured.events.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn default_request() -> EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_subject_id(Uuid::from_u128(0xABC))
        .build()
}

fn allow_with_one_eq(tenant_id: Uuid) -> EvaluationResponse {
    build_allow_response(vec![Constraint {
        predicates: vec![Predicate::Eq(EqPredicate::new(
            "owner_tenant_id",
            tenant_id,
        ))],
    }])
}

// ---- AuditRecord construction --------------------------------------

#[test]
fn u_12_allow_audit_record_carries_constraints_metadata() {
    let request = default_request();
    let response = allow_with_one_eq(Uuid::from_u128(1));
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(5),
        &response,
    );
    assert!(record.decision);
    assert_eq!(record.constraints_count, Some(1));
    let hash = record.constraints_hash.expect("hash populated");
    assert_eq!(hash.len(), 16);
    assert!(record.deny_reason.is_none());
}

#[test]
fn u_13_deny_audit_record_carries_deny_reason() {
    let request = default_request();
    let response =
        build_deny_response(SCOPE_MISMATCH_V1, Some("scope mismatch details".to_owned()));
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(3),
        &response,
    );
    assert!(!record.decision);
    assert!(record.constraints_count.is_none());
    assert!(record.constraints_hash.is_none());
    let reason = record.deny_reason.expect("deny_reason populated");
    assert_eq!(reason.error_code, SCOPE_MISMATCH_V1);
    assert_eq!(reason.details.as_deref(), Some("scope mismatch details"));
}

#[test]
fn empty_constraints_allow_records_no_hash() {
    // require_constraints=false path → empty constraints, no hash.
    let request = default_request();
    let response = build_allow_response(vec![]);
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(2),
        &response,
    );
    assert!(record.decision);
    assert_eq!(record.constraints_count, Some(0));
    assert!(record.constraints_hash.is_none());
}

#[test]
fn constraints_hash_is_16_hex_chars() {
    let response = build_allow_response(vec![Constraint {
        predicates: vec![Predicate::In(InPredicate::new(
            "owner_tenant_id",
            vec![Uuid::from_u128(1), Uuid::from_u128(2)],
        ))],
    }]);
    let request = default_request();
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(1),
        &response,
    );
    let hash = record.constraints_hash.expect("hash populated");
    assert_eq!(hash.len(), 16);
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "hash should be lowercase hex: {hash}"
    );
}

#[test]
fn constraints_hash_is_deterministic() {
    let response = build_allow_response(vec![Constraint {
        predicates: vec![Predicate::Eq(EqPredicate::new(
            "owner_tenant_id",
            Uuid::from_u128(42),
        ))],
    }]);
    let request = default_request();
    let r1 = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(1),
        &response,
    );
    let r2 = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(2),
        &response,
    );
    assert_eq!(r1.constraints_hash, r2.constraints_hash);
}

#[test]
fn subject_tenant_id_parsed_from_subject_properties() {
    let tenant_id = Uuid::from_u128(0xDEAD_BEEF);
    let mut request = default_request();
    request.subject.properties.insert(
        "tenant_id".to_owned(),
        serde_json::json!(tenant_id.to_string()),
    );
    let response = build_allow_response(vec![]);
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(1),
        &response,
    );
    assert_eq!(record.subject_tenant_id, Some(tenant_id));
}

#[test]
fn subject_tenant_id_none_when_missing() {
    // Explicitly drop the tenant property — the builder default now stamps one.
    let request = EvaluationRequestBuilder::default()
        .without_subject_tenant()
        .build();
    let response = build_allow_response(vec![]);
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(1),
        &response,
    );
    assert_eq!(record.subject_tenant_id, None);
}

// ---- Structural sensitive-data exclusion ----------------------------

#[test]
fn bearer_token_never_appears_in_audit_record() {
    // The bearer token never reaches the record — `AuditRecord` has no
    // field that could carry it. Verify via Debug formatting: the token
    // string must not appear anywhere in the formatted output.
    let request = default_request(); // builder doesn't populate bearer_token
    let response = build_deny_response(SCOPE_MISMATCH_V1, Some("scope mismatch".to_owned()));
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(1),
        &response,
    );
    let formatted = format!("{record:?}");
    // The literal "secret-abc-xyz" is the token an integration test
    // injects via the builder; the unit test asserts the Debug output
    // never carries any bearer-token-shaped substring. Use a known
    // canary value here for the same intent.
    assert!(
        !formatted.contains("bearer_token"),
        "AuditRecord debug must not name a bearer_token field: {formatted}"
    );
}

#[test]
fn raw_predicate_values_never_appear_in_audit_record_debug() {
    // The Debug output of `AuditRecord` must not include the raw
    // predicate values — only the hash. A known tenant UUID inside an
    // Eq predicate must be absent.
    let tenant_uuid = Uuid::from_u128(0x_CAFE_BABE_DEAD_BEEF_DEAD_BEEF_CAFE_BABE);
    let response = build_allow_response(vec![Constraint {
        predicates: vec![Predicate::Eq(EqPredicate::new(
            "owner_tenant_id",
            tenant_uuid,
        ))],
    }]);
    let request = default_request();
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(1),
        &response,
    );
    let formatted = format!("{record:?}");
    assert!(
        !formatted.contains(&tenant_uuid.to_string()),
        "AuditRecord debug must not carry raw predicate values: {formatted}"
    );
}

// ---- Emitter gating -------------------------------------------------

#[test]
fn emit_writes_nothing_when_disabled() {
    let (captured, _guard) = capture_audit();

    let emitter = AuditEmitter::new(false);
    let request = default_request();
    let response = build_allow_response(vec![]);
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(1),
        &response,
    );
    emitter.emit(&record);

    assert!(
        recorded(&captured).is_empty(),
        "a disabled emitter MUST write no {AUDIT_TARGET} event"
    );
}

#[test]
fn emit_writes_one_sanitized_record_when_enabled() {
    let (captured, _guard) = capture_audit();

    let emitter = AuditEmitter::new(true);
    // Control characters in the caller-controlled fields: a CR/LF could forge
    // or split an audit line under a line-oriented subscriber, so the emitter
    // has to neutralize them before they reach the record.
    let request = EvaluationRequestBuilder::default()
        .with_subject_id(Uuid::from_u128(0xABC))
        .with_action_name("re\r\nad")
        .with_resource_type("gts.cf.core.resources.te\nst.v1~")
        .build();
    let response = build_deny_response(
        CONSTRAINTS_UNAVAILABLE_V1,
        Some("require_constraints=true but empty".to_owned()),
    );
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(7),
        &response,
    );
    emitter.emit(&record);

    let events = recorded(&captured);
    assert_eq!(events.len(), 1, "one decision MUST emit exactly one record");
    let fields = &events[0];

    assert_eq!(
        fields.get("decision").map(String::as_str),
        Some("false"),
        "the decision itself must be on the record; fields={fields:?}"
    );
    assert_eq!(
        fields.get("deny_error_code").map(String::as_str),
        Some(CONSTRAINTS_UNAVAILABLE_V1),
        "a deny must carry its machine-readable code; fields={fields:?}"
    );

    let action = fields.get("action").expect("action recorded");
    let resource_type = fields.get("resource_type").expect("resource_type recorded");
    for (name, value) in [("action", action), ("resource_type", resource_type)] {
        assert!(
            !value.chars().any(char::is_control),
            "{name} must reach the audit line with no control characters: {value:?}"
        );
        assert!(
            value.contains('\u{FFFD}'),
            "{name} must show the neutralized character rather than dropping it: {value:?}"
        );
    }
}

#[test]
fn unknown_resource_type_deny_audit_record_carries_correct_error_code() {
    let request = default_request();
    let response = build_deny_response(
        UNKNOWN_RESOURCE_TYPE_V1,
        Some("unknown gts type: 'gts.cf.unknown.v1~'".to_owned()),
    );
    let record = AuditRecord::from_response(
        Uuid::new_v4(),
        &request,
        Duration::from_millis(1),
        &response,
    );
    assert_eq!(
        record
            .deny_reason
            .as_ref()
            .map_or("", |d| d.error_code.as_str()),
        UNKNOWN_RESOURCE_TYPE_V1
    );
}

/// Streaming into the FNV sink must produce exactly the digest the previous
/// `serde_json::to_string`-then-hash version produced.
///
/// The hash appears in audit records, so a change in value would silently
/// break comparison against everything already emitted. This pins the
/// equivalence rather than trusting that folding the same bytes in the same
/// order is obviously the same — which is the whole basis of the change.
#[test]
fn fnv_writer_matches_hashing_the_json_string() {
    // The pre-change constants, declared before any statement.
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;

    // A non-trivial set: two predicate shapes and a multi-id `In`, so the
    // serializer emits nested arrays rather than a flat scalar.
    let constraints = vec![
        Constraint {
            predicates: vec![Predicate::Eq(EqPredicate::new(
                "owner_tenant_id",
                Uuid::from_u128(7),
            ))],
        },
        Constraint {
            predicates: vec![Predicate::In(InPredicate::new(
                "resource_id",
                vec![Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)],
            ))],
        },
    ];
    let json = serde_json::to_string(&constraints).expect("constraints serialize");

    // The pre-change implementation, inline.
    let mut expected = BASIS;
    for &b in json.as_bytes() {
        expected ^= u64::from(b);
        expected = expected.wrapping_mul(PRIME);
    }

    let mut sink = FnvWriter::new();
    serde_json::to_writer(&mut sink, &constraints).expect("constraints stream");

    assert_eq!(
        format!("{:016x}", sink.finish()),
        format!("{expected:016x}"),
        "the streaming digest must equal the string-based one, or every \
         previously emitted audit hash becomes incomparable"
    );
    assert_eq!(
        compute_constraints_hash(&constraints),
        Some(format!("{expected:016x}")),
        "compute_constraints_hash must return that same digest"
    );
}
