#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::field_reassign_with_default,
    clippy::needless_pass_by_value
)]
use super::*;
use crate::domain::clock::StubClock;
use crate::domain::subject_type::TrustedSystemActors;
use crate::test_support::RecordingTypesRegistry;

const KNOWN_TYPE: &str = "gts.cf.core.security.subject_user.v1~";
const UNKNOWN_TYPE: &str = "gts.cf.unknown.v1~";
const TTL: Duration = Duration::from_mins(1);

fn validator_with(
    mode: GtsValidationMode,
    registry: Arc<RecordingTypesRegistry>,
    clock: Arc<StubClock>,
) -> GtsTypeValidator {
    GtsTypeValidator::new(
        mode,
        registry as Arc<dyn TypesRegistryClient>,
        TTL,
        clock as Arc<dyn Clock>,
    )
}

fn known_registry() -> Arc<RecordingTypesRegistry> {
    Arc::new(RecordingTypesRegistry::with_known_types(vec![KNOWN_TYPE]))
}

fn expect_allow(outcome: TypeValidationOutcome) {
    assert!(
        matches!(outcome, TypeValidationOutcome::Allow),
        "expected Allow, got {outcome:?}"
    );
}

fn expect_deny_with(outcome: TypeValidationOutcome, expected_code: &str) -> EvaluationResponse {
    match outcome {
        TypeValidationOutcome::Deny(response) => {
            let reason = response
                .context
                .deny_reason
                .clone()
                .expect("deny carries deny_reason");
            assert_eq!(reason.error_code, expected_code);
            response
        }
        TypeValidationOutcome::Allow => panic!("expected Deny, got Allow"),
    }
}

// ---- Mode behavior ---------------------------------------------------

#[tokio::test]
async fn u_50_strict_known_type_returns_allow() {
    let registry = known_registry();
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, Arc::clone(&registry), clock);

    let outcome = validator
        .validate_type(KNOWN_TYPE, "resource")
        .await
        .expect("known type in Strict mode -> Ok(Allow)");
    expect_allow(outcome);
}

#[tokio::test]
async fn u_51_strict_unknown_type_returns_deny() {
    let registry = known_registry();
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, registry, clock);

    let outcome = validator
        .validate_type(UNKNOWN_TYPE, "resource")
        .await
        .expect("Strict + Unknown is Ok(Deny), not Err");
    let response = expect_deny_with(outcome, UNKNOWN_RESOURCE_TYPE_V1);
    let details = response
        .context
        .deny_reason
        .and_then(|d| d.details)
        .expect("details populated");
    assert!(
        details.contains(&format!("unknown resource gts type: '{UNKNOWN_TYPE}'")),
        "details should name the offending type and its kind: {details}"
    );
}

#[tokio::test]
async fn u_52_strict_registry_unavailable_returns_service_unavailable() {
    let registry = Arc::new(RecordingTypesRegistry::new());
    registry.set_unavailable(true);
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, registry, clock);

    match validator.validate_type(KNOWN_TYPE, "resource").await {
        Err(err @ PluginError::GtsRegistryUnavailable) => {
            assert_eq!(err.to_string(), "gts schema registry unavailable");
        }
        other => panic!("expected Err(GtsRegistryUnavailable), got {other:?}"),
    }
}

#[tokio::test]
async fn u_53_warn_unknown_type_returns_allow() {
    let registry = known_registry();
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Warn, registry, clock);

    let outcome = validator
        .validate_type(UNKNOWN_TYPE, "resource")
        .await
        .expect("Warn mode + unknown -> Ok(Allow)");
    expect_allow(outcome);
}

/// `Warn` tolerates an incomplete type REGISTRATION, not a registry that is
/// down. Allowing here let every request ride through unvalidated for the whole
/// outage, which is the one case `Warn` must not cover.
#[tokio::test]
async fn u_54_warn_registry_unavailable_still_errors() {
    let registry = Arc::new(RecordingTypesRegistry::new());
    registry.set_unavailable(true);
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Warn, registry, clock);

    match validator.validate_type(KNOWN_TYPE, "resource").await {
        Err(err @ PluginError::GtsRegistryUnavailable) => {
            assert_eq!(err.to_string(), "gts schema registry unavailable");
        }
        other => panic!("Warn + registry outage MUST fail closed, got {other:?}"),
    }
}

#[tokio::test]
async fn u_55_off_mode_returns_allow_without_registry_call() {
    let registry = Arc::new(RecordingTypesRegistry::new());
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Off, Arc::clone(&registry), clock);

    let outcome = validator
        .validate_type("any.id.even.malformed", "resource")
        .await
        .expect("Off mode short-circuits to Ok(Allow)");
    expect_allow(outcome);
    assert_eq!(
        registry.get_type_schema_call_count(),
        0,
        "Off mode must not invoke get_type_schema"
    );
}

// ---- Cache behavior --------------------------------------------------

#[tokio::test]
async fn u_56_cache_hit_avoids_registry_call() {
    let registry = known_registry();
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, Arc::clone(&registry), clock);

    validator
        .validate_type(KNOWN_TYPE, "resource")
        .await
        .unwrap();
    assert_eq!(registry.get_type_schema_call_count(), 1);

    validator
        .validate_type(KNOWN_TYPE, "resource")
        .await
        .unwrap();
    assert_eq!(
        registry.get_type_schema_call_count(),
        1,
        "second call must be a cache hit"
    );
}

#[tokio::test]
async fn cache_hit_serves_through_registry_outage() {
    let registry = known_registry();
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, Arc::clone(&registry), clock);

    // Warm the cache while registry is up.
    validator
        .validate_type(KNOWN_TYPE, "resource")
        .await
        .unwrap();
    let calls_after_warm = registry.get_type_schema_call_count();

    // Take the registry down. The cached entry must still serve.
    registry.set_unavailable(true);
    validator
        .validate_type(KNOWN_TYPE, "resource")
        .await
        .unwrap();
    assert_eq!(
        registry.get_type_schema_call_count(),
        calls_after_warm,
        "cached entry must not trigger a registry call"
    );
}

#[tokio::test]
async fn cache_ttl_expires_on_read_and_forces_refetch() {
    let registry = known_registry();
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(
        GtsValidationMode::Strict,
        Arc::clone(&registry),
        Arc::clone(&clock),
    );

    validator
        .validate_type(KNOWN_TYPE, "resource")
        .await
        .unwrap();
    assert_eq!(registry.get_type_schema_call_count(), 1);

    // Advance past TTL — the next call must refetch.
    clock.advance(TTL + Duration::from_secs(1));
    validator
        .validate_type(KNOWN_TYPE, "resource")
        .await
        .unwrap();
    assert_eq!(
        registry.get_type_schema_call_count(),
        2,
        "expired entry must trigger a refetch"
    );
}

#[tokio::test]
async fn registry_unavailable_is_not_cached() {
    let registry = Arc::new(RecordingTypesRegistry::new());
    registry.set_unavailable(true);
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Warn, Arc::clone(&registry), clock);

    // First call: registry down → the outage errors in every mode that
    // consults the registry. What this test pins is that nothing was cached.
    validator
        .validate_type(KNOWN_TYPE, "resource")
        .await
        .expect_err("a registry outage is an infrastructure error");
    assert_eq!(registry.get_type_schema_call_count(), 1);

    // Flip registry back up + add the type. Second call must re-query
    // (no stale RegistryUnavailable entry was cached).
    registry.set_unavailable(false);
    registry.add_known_type(KNOWN_TYPE);
    validator
        .validate_type(KNOWN_TYPE, "resource")
        .await
        .unwrap();
    assert_eq!(
        registry.get_type_schema_call_count(),
        2,
        "RegistryUnavailable must not be cached"
    );
}

#[tokio::test]
async fn lru_eviction_forces_refetch() {
    // Strict mode + Warn registry behavior so we can exercise the
    // eviction path without errors.
    let registry = Arc::new(RecordingTypesRegistry::new());
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Warn, Arc::clone(&registry), clock);

    // Fill the cache: 1024 distinct Unknown entries + one final entry
    // that should evict the very first.
    for i in 0..=CACHE_CAPACITY {
        let id = format!("gts.test.type_{i}.v1~");
        validator.validate_type(&id, "resource").await.unwrap();
    }
    assert_eq!(
        registry.get_type_schema_call_count(),
        CACHE_CAPACITY + 1,
        "every distinct lookup must reach the registry"
    );

    // The first id is now evicted. A fresh lookup must hit the registry
    // again — call count increments.
    validator
        .validate_type("gts.test.type_0.v1~", "resource")
        .await
        .unwrap();
    assert_eq!(
        registry.get_type_schema_call_count(),
        CACHE_CAPACITY + 2,
        "evicted entry must refetch"
    );
}

// ---- validate_request behavior ---------------------------------------

fn request_with_types(subject: &str, resource: &str) -> EvaluationRequest {
    use crate::test_support::EvaluationRequestBuilder;
    EvaluationRequestBuilder::default()
        .with_subject_type(Some(subject.to_owned()))
        .with_resource_type(resource.to_owned())
        .build()
}

#[tokio::test]
async fn u_57_validate_request_validates_subject_then_resource() {
    let subject = "gts.cf.core.security.subject_user.v1~";
    let resource = "gts.cf.core.resources.test.v1~";
    let registry = Arc::new(RecordingTypesRegistry::with_known_types(vec![
        subject, resource,
    ]));
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, Arc::clone(&registry), clock);

    let outcome = validator
        .validate_request(
            &request_with_types(subject, resource),
            &TrustedSystemActors::default(),
        )
        .await
        .expect("both types Known -> Ok(Allow)");
    expect_allow(outcome);
    assert_eq!(
        registry.get_type_schema_call_count(),
        2,
        "subject + resource = 2 lookups"
    );
}

#[tokio::test]
async fn validate_request_fail_fast_on_subject() {
    let resource = "gts.cf.core.resources.test.v1~";
    let registry = Arc::new(RecordingTypesRegistry::with_known_types(vec![resource]));
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, Arc::clone(&registry), clock);

    let outcome = validator
        .validate_request(
            &request_with_types(UNKNOWN_TYPE, resource),
            &TrustedSystemActors::default(),
        )
        .await
        .expect("subject failure is Ok(Deny), not Err");
    let response = expect_deny_with(outcome, UNKNOWN_RESOURCE_TYPE_V1);
    let details = response
        .context
        .deny_reason
        .and_then(|d| d.details)
        .expect("details populated");
    assert!(
        details.contains(&format!("unknown subject gts type: '{UNKNOWN_TYPE}'")),
        "details must name BOTH the kind (subject) and the offending type so the \
         unified unknown_resource_type.v1 code is not misleading: {details}"
    );
    assert_eq!(
        registry.get_type_schema_call_count(),
        1,
        "subject failure must short-circuit before resource lookup"
    );
}

#[tokio::test]
async fn validate_request_resource_error_after_subject_passes() {
    let subject = "gts.cf.core.security.subject_user.v1~";
    let unknown_resource = "gts.cf.resources.bogus.v1~";
    let registry = Arc::new(RecordingTypesRegistry::with_known_types(vec![subject]));
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, Arc::clone(&registry), clock);

    let outcome = validator
        .validate_request(
            &request_with_types(subject, unknown_resource),
            &TrustedSystemActors::default(),
        )
        .await
        .expect("resource failure is Ok(Deny), not Err");
    let response = expect_deny_with(outcome, UNKNOWN_RESOURCE_TYPE_V1);
    let details = response
        .context
        .deny_reason
        .and_then(|d| d.details)
        .expect("details populated");
    assert!(
        details.contains(unknown_resource),
        "details should name the resource type: {details}"
    );
    assert_eq!(
        registry.get_type_schema_call_count(),
        2,
        "both lookups must run (subject passes; resource fails)"
    );
}

#[tokio::test]
async fn validate_request_tolerates_absent_subject_type() {
    // Absent subject_type is valid — it defaults to `User` in policy evaluation
    // (mirrors RBAC), so the GTS subject-type check is skipped and only the
    // resource type is validated. Must NOT fail closed on the missing tag.
    use crate::test_support::EvaluationRequestBuilder;

    let registry = known_registry();
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, registry, clock);

    let request = EvaluationRequestBuilder::default()
        .with_subject_type(None)
        .with_resource_type(KNOWN_TYPE.to_owned())
        .build();
    let outcome = validator
        .validate_request(&request, &TrustedSystemActors::default())
        .await
        .expect("absent subject_type is skipped, not an error");
    match outcome {
        TypeValidationOutcome::Allow => {}
        deny @ TypeValidationOutcome::Deny(_) => {
            panic!("expected Allow (subject skipped, known resource), got {deny:?}")
        }
    }
}

/// A trusted system actor's `subject_type` is a private in-process marker, not
/// a registered GTS type, so the registry answers "unknown" for it. Before the
/// skip, `mode: strict` denied the actor here — before the trusted-allow in
/// policy evaluation could ever run — which made `trusted_system_actors` and
/// `gts_validation.mode: strict` silently non-composable.
#[tokio::test]
async fn validate_request_skips_the_subject_leg_for_a_trusted_actor_under_strict() {
    use crate::test_support::trusted_actors::{
        AM_SYSTEM_ACTOR_UUID, AM_SYSTEM_SUBJECT_TYPE, trusted_actors,
    };

    let resource = "gts.cf.core.resources.test.v1~";
    // The registry knows the resource type and nothing else: the actor's tag
    // would resolve to Unknown -> Deny if the subject leg still ran.
    let registry = Arc::new(RecordingTypesRegistry::with_known_types(vec![resource]));
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, Arc::clone(&registry), clock);

    let request = crate::test_support::EvaluationRequestBuilder::default()
        .with_subject_id(AM_SYSTEM_ACTOR_UUID)
        .with_subject_type(Some(AM_SYSTEM_SUBJECT_TYPE.to_owned()))
        .with_resource_type(resource.to_owned())
        .build();

    let outcome = validator
        .validate_request(&request, &trusted_actors())
        .await
        .expect("a trusted actor must not be denied by GTS validation");
    expect_allow(outcome);
    assert_eq!(
        registry.get_type_schema_call_count(),
        1,
        "only the resource leg may reach the registry for a trusted actor"
    );
}

/// The skip is keyed on the (`subject_id`, `subject_type`) PAIR, so borrowing a
/// trusted actor's tag from an untrusted id does not buy the bypass.
#[tokio::test]
async fn validate_request_does_not_skip_a_forged_trusted_subject_type() {
    use crate::test_support::trusted_actors::{AM_SYSTEM_SUBJECT_TYPE, trusted_actors};

    let resource = "gts.cf.core.resources.test.v1~";
    let registry = Arc::new(RecordingTypesRegistry::with_known_types(vec![resource]));
    let clock = Arc::new(StubClock::new());
    let validator = validator_with(GtsValidationMode::Strict, Arc::clone(&registry), clock);

    let request = crate::test_support::EvaluationRequestBuilder::default()
        .with_subject_id(uuid::Uuid::from_u128(0xdead_beef))
        .with_subject_type(Some(AM_SYSTEM_SUBJECT_TYPE.to_owned()))
        .with_resource_type(resource.to_owned())
        .build();

    let outcome = validator
        .validate_request(&request, &trusted_actors())
        .await
        .expect("an unknown subject type is a business deny under strict");
    expect_deny_with(outcome, UNKNOWN_RESOURCE_TYPE_V1);
}
