//! Regression tests pinning the load-bearing invariant of
//! `Service::create_usage_records` PDP dedup:
//!
//! > **For every record, the PDP request composed by
//! > [`authorize_usage_record`] is byte-identical to the one composed by
//! > [`authorize_attribution_tuple`] when fed
//! > [`AttributionTupleKey::from_record(record)`].**
//!
//! If that invariant ever breaks, two records that
//! [`AttributionTupleKey`] groups together could be judged differently
//! by the PDP — a bypass. The structural prevention (the per-tuple
//! composer takes only `&AttributionTupleKey`, so no record field can
//! sneak in) makes the divergence physically impossible at the source
//! level; these tests are the corresponding behavioral pin so any
//! refactor that accidentally re-introduces a `&UsageRecord` dependency
//! in the composer is caught by the suite.

use std::collections::BTreeMap;
use std::sync::Arc;
use toolkit_gts::gts_id;

use authz_resolver_sdk::models::EvaluationRequest;
use rust_decimal::Decimal;
use time::OffsetDateTime;
use toolkit_security::SecurityContext;
use usage_collector_sdk::{
    IdempotencyKey, ResourceRef, SubjectRef, UsageRecord, UsageRecordStatus, UsageTypeGtsId,
};
use uuid::Uuid;

use super::{
    AttributionTupleKey, authorize_attribution_tuple, authorize_usage_record, usage_record,
};
use crate::domain::ports::metrics::{NoopMetrics, PdpOp};
use crate::domain::test_support::{CapturingTenantPermitResolver, enforcer_for};

const SAMPLE_GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

/// Arbitrary RG member-handle type for group-filter fixtures — usage-collector
/// rejects group predicates regardless of the type, so only the shape matters.
const SAMPLE_MEMBER_TYPE: &str = gts_id!("cf.core.rg.type.v1~example.core.uc.owner.v1~");

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0xA110))
        .subject_tenant_id(Uuid::from_u128(0xB220))
        .subject_type("user")
        .build()
        .expect("authenticated ctx")
}

fn record_with(subject: Option<SubjectRef>) -> UsageRecord {
    UsageRecord {
        id: Uuid::from_u128(0x0001),
        gts_id: UsageTypeGtsId::new(SAMPLE_GTS_ID).expect("valid gts_id"),
        tenant_id: Uuid::from_u128(0xC330),
        resource_ref: ResourceRef::new("rsc-eq", "compute.vm").expect("valid resource ref"),
        subject_ref: subject,
        metadata: BTreeMap::new(),
        value: Decimal::from(1),
        idempotency_key: IdempotencyKey::new("idem-eq").expect("valid idempotency key"),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        created_at: OffsetDateTime::UNIX_EPOCH,
    }
}

/// A record identical to [`record_with(None)`] but owned by `tenant_id`, for
/// driving the per-record tenant gate against a specific owning tenant.
fn record_with_tenant(tenant_id: Uuid) -> UsageRecord {
    UsageRecord {
        tenant_id,
        ..record_with(None)
    }
}

/// Run both PDP composers against the same record and return the
/// captured `EvaluationRequest`s as JSON values (for stable, transitive
/// equality across `HashMap`-backed property bags).
async fn captured_requests_for(record: &UsageRecord) -> (serde_json::Value, serde_json::Value) {
    let resolver = CapturingTenantPermitResolver::new();
    let enforcer =
        enforcer_for(Arc::clone(&resolver) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    authorize_usage_record(
        &enforcer,
        &NoopMetrics,
        PdpOp::Ingest,
        &ctx(),
        record,
        usage_record::actions::CREATE,
    )
    .await
    .expect("permit");
    let from_record = resolver.take_last_request().expect("first call captured");

    let key = AttributionTupleKey::from_record(record, usage_record::actions::CREATE);
    authorize_attribution_tuple(&enforcer, &NoopMetrics, PdpOp::Ingest, &ctx(), &key)
        .await
        .expect("permit");
    let from_key = resolver.take_last_request().expect("second call captured");

    (json(&from_record), json(&from_key))
}

fn json(req: &EvaluationRequest) -> serde_json::Value {
    serde_json::to_value(req).expect("EvaluationRequest serializes as JSON")
}

/// Subject-absent: the tuple key carries no subject fields, the request
/// MUST carry no subject `resource_property` either.
#[tokio::test]
async fn key_and_record_compose_byte_identical_pdp_requests_without_subject() {
    let record = record_with(None);
    let (from_record, from_key) = captured_requests_for(&record).await;
    assert_eq!(
        from_record, from_key,
        "PDP request composed from the record MUST equal the one composed from \
         AttributionTupleKey::from_record(record) -- otherwise the dedup grouping in \
         Service::create_usage_records can collapse records the PDP would have \
         judged differently. Drift detected for subject-absent record.",
    );
}

/// Subject present without `subject_type`: only `OWNER_ID` is contributed.
#[tokio::test]
async fn key_and_record_compose_byte_identical_pdp_requests_with_subject_id_only() {
    let record = record_with(Some(
        SubjectRef::new("subject-eq-1", None::<String>).expect("valid subject"),
    ));
    let (from_record, from_key) = captured_requests_for(&record).await;
    assert_eq!(
        from_record, from_key,
        "drift detected for subject-id-only record"
    );
}

/// Subject present WITH `subject_type`: both `OWNER_ID` and
/// `SUBJECT_TYPE` are contributed. This is the maximal-attribute path —
/// drift here would be the worst case.
#[tokio::test]
async fn key_and_record_compose_byte_identical_pdp_requests_with_full_subject() {
    let record = record_with(Some(
        SubjectRef::new("subject-eq-2", Some("service")).expect("valid subject"),
    ));
    let (from_record, from_key) = captured_requests_for(&record).await;
    assert_eq!(
        from_record, from_key,
        "drift detected for subject-with-type record"
    );
}

/// Two records that hash-equal under `AttributionTupleKey` MUST always
/// produce equal PDP requests -- even when their *non*-tuple fields
/// (`id`, `gts_id`, `value`, `idempotency_key`, `metadata`,
/// `corrects_id`, `created_at`) differ wildly. This pins the
/// projection-correctness premise of the dedup directly: "share the
/// tuple => share the PDP payload".
#[tokio::test]
async fn equal_tuple_keys_produce_equal_pdp_requests_even_when_non_tuple_fields_differ() {
    let record_a = UsageRecord {
        id: Uuid::from_u128(0xAAAA),
        gts_id: UsageTypeGtsId::new(SAMPLE_GTS_ID).expect("valid gts_id"),
        tenant_id: Uuid::from_u128(0xDEAD),
        resource_ref: ResourceRef::new("rsc-shared", "compute.vm").expect("valid resource ref"),
        subject_ref: Some(SubjectRef::new("sub-shared", Some("user")).expect("valid subject")),
        metadata: BTreeMap::new(),
        value: Decimal::from(1),
        idempotency_key: IdempotencyKey::new("idem-A").expect("valid idempotency key"),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        created_at: OffsetDateTime::UNIX_EPOCH,
    };
    let record_b = UsageRecord {
        // Same tuple-key fields …
        tenant_id: record_a.tenant_id,
        resource_ref: record_a.resource_ref.clone(),
        subject_ref: record_a.subject_ref.clone(),
        // … wildly different non-tuple fields:
        id: Uuid::from_u128(0xBBBB),
        gts_id: UsageTypeGtsId::new(SAMPLE_GTS_ID).expect("valid gts_id"),
        metadata: BTreeMap::new(),
        value: Decimal::from(-999),
        idempotency_key: IdempotencyKey::new("idem-B-different").expect("valid idempotency key"),
        corrects_id: Some(Uuid::from_u128(0xCCCC)),
        status: UsageRecordStatus::Active,
        created_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(24),
    };

    let key_a = AttributionTupleKey::from_record(&record_a, usage_record::actions::CREATE);
    let key_b = AttributionTupleKey::from_record(&record_b, usage_record::actions::CREATE);
    assert_eq!(
        key_a, key_b,
        "test premise: records were constructed to share the attribution tuple",
    );

    let resolver = CapturingTenantPermitResolver::new();
    let enforcer =
        enforcer_for(Arc::clone(&resolver) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    authorize_usage_record(
        &enforcer,
        &NoopMetrics,
        PdpOp::Ingest,
        &ctx(),
        &record_a,
        usage_record::actions::CREATE,
    )
    .await
    .expect("permit");
    let req_a = json(&resolver.take_last_request().expect("captured A"));

    authorize_usage_record(
        &enforcer,
        &NoopMetrics,
        PdpOp::Ingest,
        &ctx(),
        &record_b,
        usage_record::actions::CREATE,
    )
    .await
    .expect("permit");
    let req_b = json(&resolver.take_last_request().expect("captured B"));

    assert_eq!(
        req_a, req_b,
        "two records that hash-equal under AttributionTupleKey MUST compose \
         identical PDP EvaluationRequests; any per-record field leaking into \
         the PDP payload defeats the dedup's safety property",
    );
}

/// Same attribution attributes, different `action` MUST NOT hash-equal.
#[test]
fn different_actions_yield_distinct_tuple_keys_for_same_attribution() {
    let record = record_with(Some(
        SubjectRef::new("sub-action", Some("user")).expect("valid subject"),
    ));
    let create = AttributionTupleKey::from_record(&record, usage_record::actions::CREATE);
    let deactivate = AttributionTupleKey::from_record(&record, usage_record::actions::DEACTIVATE);
    assert_ne!(
        create, deactivate,
        "action MUST participate in AttributionTupleKey hash/eq; \
         otherwise a batch mixing CREATE and DEACTIVATE for the same \
         tuple would share a single PDP decision and silently bypass \
         per-action policy",
    );
}

/// Integration pin: the per-record tenant gate is actually WIRED into
/// [`authorize_attribution_tuple`], not merely unit-tested in isolation via
/// `scope_admits_tenant`. The tenant-echoing happy-path fakes can never produce
/// a scope that fails the gate, so without this test a regression that dropped
/// the gate call — re-opening the cross-tenant bypass this change closes —
/// would pass the whole gear suite. Here the PDP permits but scopes the grant
/// to ONE tenant: a record owned by a different tenant MUST be denied, and the
/// granted tenant MUST be permitted. (Unit coverage of the gate's full-tuple
/// logic lives in `attribution_gate_tests`.)
#[tokio::test]
async fn authorize_attribution_tuple_denies_record_outside_granted_tenant() {
    use toolkit_security::pep_properties;

    use crate::domain::DomainError;
    use crate::domain::test_support::CountingPermitResolver;

    let granted = Uuid::from_u128(0x6001);
    let foreign = Uuid::from_u128(0x6002);
    // Permit, but scope the grant to exactly `granted` (independent of the
    // request) — models a `/tenants/{granted}`-scoped caller.
    let resolver =
        CountingPermitResolver::new(pep_properties::OWNER_TENANT_ID, granted.to_string());
    let enforcer = enforcer_for(resolver as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);

    let foreign_key = AttributionTupleKey::from_record(
        &record_with_tenant(foreign),
        usage_record::actions::CREATE,
    );
    let denied =
        authorize_attribution_tuple(&enforcer, &NoopMetrics, PdpOp::Ingest, &ctx(), &foreign_key)
            .await;
    assert!(
        matches!(denied, Err(DomainError::AuthorizationDenied { .. })),
        "a record owned by a tenant outside the PDP-granted scope MUST be denied, got {denied:?}",
    );

    let granted_key = AttributionTupleKey::from_record(
        &record_with_tenant(granted),
        usage_record::actions::CREATE,
    );
    authorize_attribution_tuple(&enforcer, &NoopMetrics, PdpOp::Ingest, &ctx(), &granted_key)
        .await
        .expect("a record owned by the granted tenant is permitted");
}

// ---------------------------------------------------------------------------
// scope_to_odata_filter — projects AccessScope into ODataQuery filter
// ---------------------------------------------------------------------------

#[cfg(test)]
mod scope_to_odata_tests {
    use super::SAMPLE_MEMBER_TYPE;
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    use toolkit_security::{
        AccessScope, InGroupScopeFilter, InTenantSubtreeScopeFilter, ScopeConstraint, ScopeFilter,
        ScopeValue, pep_properties,
    };
    use uuid::Uuid;

    use crate::domain::DomainError;
    use crate::domain::authz::{scope_to_odata_filter, usage_record};

    /// Helper — flatten an Expr to a debuggable s-expression string so
    /// assertions can read at a glance.
    fn fmt_expr(expr: &Expr) -> String {
        match expr {
            Expr::And(a, b) => format!("(and {} {})", fmt_expr(a), fmt_expr(b)),
            Expr::Or(a, b) => format!("(or {} {})", fmt_expr(a), fmt_expr(b)),
            Expr::Not(a) => format!("(not {})", fmt_expr(a)),
            Expr::Compare(lhs, op, rhs) => {
                let op = match op {
                    CompareOperator::Eq => "eq",
                    CompareOperator::Ne => "ne",
                    CompareOperator::Gt => "gt",
                    CompareOperator::Ge => "ge",
                    CompareOperator::Lt => "lt",
                    CompareOperator::Le => "le",
                };
                format!("({} {} {})", op, fmt_expr(lhs), fmt_expr(rhs))
            }
            Expr::In(lhs, vs) => {
                let vs: Vec<_> = vs.iter().map(fmt_expr).collect();
                format!("(in {} [{}])", fmt_expr(lhs), vs.join(" "))
            }
            Expr::Function(name, args) => {
                let args: Vec<_> = args.iter().map(fmt_expr).collect();
                format!("({} {})", name, args.join(" "))
            }
            Expr::Identifier(id) => id.clone(),
            Expr::Value(v) => match v {
                Value::Uuid(u) => format!("uuid:{u}"),
                Value::String(s) => format!("\"{s}\""),
                Value::Bool(b) => format!("{b}"),
                Value::Number(n) => format!("{n}"),
                Value::DateTime(t) => format!("dt:{t}"),
                Value::Date(d) => format!("date:{d}"),
                Value::Time(t) => format!("time:{t}"),
                Value::Null => "null".to_owned(),
            },
        }
    }

    fn uid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    #[test]
    fn unconstrained_scope_is_denied_fail_closed() {
        // Under `require_constraints(true)` a legitimate LIST/aggregate permit
        // always carries `OWNER_TENANT_ID In [..]` narrowing (admin included).
        // An `allow_all` scope only arises from a degenerate empty-predicate
        // permit (compiler.rs returns `allow_all()` when every compiled
        // constraint is empty), so emitting "no row narrowing" here would leak
        // every tenant's records. It MUST fail closed, mirroring the per-record
        // gate's `scope_admits_attribution_tuple`.
        let scope = AccessScope::allow_all();
        let err = scope_to_odata_filter(&scope).expect_err("allow_all -> authz denied");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn empty_constraint_disjunct_is_denied_fail_closed() {
        // A constraint with no filters matches every row (an allow-all
        // disjunct). Honouring it would widen the projection to all tenants,
        // so it MUST fail closed rather than collapse to "no row narrowing".
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![])]);
        assert!(!scope.is_unconstrained(), "guard: not the allow_all path");
        assert!(!scope.is_deny_all(), "guard: not the deny_all path");
        let err = scope_to_odata_filter(&scope).expect_err("empty constraint -> authz denied");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn deny_all_scope_lifts_to_authorization_denied() {
        let scope = AccessScope::deny_all();
        let err = scope_to_odata_filter(&scope).expect_err("deny_all -> authz denied");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn single_eq_constraint_projects_to_eq_compare() {
        let tenant = uid(0xAA);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            tenant,
        )]));
        let expr = scope_to_odata_filter(&scope).expect("happy path");
        assert_eq!(
            fmt_expr(&expr),
            format!("(eq tenant_id uuid:{tenant})"),
            "OWNER_TENANT_ID must project to `tenant_id eq <uuid>`",
        );
    }

    #[test]
    fn single_in_constraint_projects_to_in_expression() {
        let t1 = uid(1);
        let t2 = uid(2);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
            pep_properties::OWNER_TENANT_ID,
            vec![t1, t2],
        )]));
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(
            fmt_expr(&expr),
            format!("(in tenant_id [uuid:{t1} uuid:{t2}])"),
        );
    }

    #[test]
    fn multi_filter_constraint_ands_within_a_constraint() {
        let tenant = uid(0xA);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_TYPE, "compute.vm"),
        ]));
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(
            fmt_expr(&expr),
            format!("(and (eq tenant_id uuid:{tenant}) (eq resource_type \"compute.vm\"))"),
        );
    }

    #[test]
    fn multi_constraint_scope_ors_at_top_level() {
        let t1 = uid(11);
        let t2 = uid(22);
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t1)]),
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t2)]),
        ]);
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(
            fmt_expr(&expr),
            format!("(or (eq tenant_id uuid:{t1}) (eq tenant_id uuid:{t2}))"),
        );
    }

    #[test]
    fn tree_predicates_fail_closed() {
        let tenant = uid(0xBEEF);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::InTenantSubtree(
            InTenantSubtreeScopeFilter::new(pep_properties::OWNER_TENANT_ID, tenant),
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("tree filter -> deny");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));

        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::InGroup(
            InGroupScopeFilter::new_typed(
                "owner_id",
                SAMPLE_MEMBER_TYPE,
                vec![ScopeValue::Uuid(uid(1))],
            ),
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("InGroup -> deny");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn unknown_pep_property_fails_closed() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            "unsupported_prop",
            "x",
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("unknown prop -> deny");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn type_mismatch_on_value_fails_closed() {
        // OWNER_TENANT_ID is UUID-typed; a string-typed value is a type
        // mismatch and MUST fail closed rather than silently coercing.
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::String("not-a-uuid".into()),
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("string->uuid mismatch");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn uuid_carried_as_string_is_accepted() {
        // The Compiler may emit a UUID as a string ScopeValue; the
        // projection MUST accept it as long as the string parses as a
        // valid UUID (mirrors `ScopeValue::as_uuid`'s convention).
        let t1 = uid(0x1234);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::String(t1.to_string()),
        )]));
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(fmt_expr(&expr), format!("(eq tenant_id uuid:{t1})"));
    }

    #[test]
    fn string_pep_property_projects_to_string_value() {
        // A non-tenant string property projects to a string value. Pair it with
        // the OWNER_TENANT_ID pinning every legitimate LIST constraint carries:
        // a tenant-less constraint now fails closed (see
        // `constraint_without_owner_tenant_filter_is_denied`).
        let t = uid(0xAA);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_SUBJECT_TYPE, "user"),
        ]));
        let expr = scope_to_odata_filter(&scope).unwrap();
        assert_eq!(
            fmt_expr(&expr),
            format!("(and (eq tenant_id uuid:{t}) (eq subject_type \"user\"))"),
        );
    }

    #[test]
    fn uuid_value_on_string_field_projects_to_canonical_string() {
        // A String-typed field (`resource_id`) carrying a `ScopeValue::Uuid` MUST
        // be accepted and rendered to its canonical string — both gates now
        // share `coerce_scope_value`, whose policy is that a UUID-shaped
        // `resource_id` matches regardless of how the compiler typed it. The
        // LIST projection previously fail-closed denied this, drifting from the
        // per-record gate (RUST-API-001 / review finding #2). Tenant-pinned so
        // the constraint clears `constraint_to_odata_conjunction`'s
        // tenant-narrowing requirement and exercises the `resource_id` coercion.
        let t = uid(0xAA);
        let u = uid(0x1357);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, ScopeValue::Uuid(u)),
        ]));
        let expr = scope_to_odata_filter(&scope).expect("uuid-on-string field accepted");
        assert_eq!(
            fmt_expr(&expr),
            format!("(and (eq tenant_id uuid:{t}) (eq resource_id \"{u}\"))"),
        );
    }

    #[test]
    fn constraint_without_owner_tenant_filter_is_denied() {
        // A non-empty constraint that narrows ONLY by a non-tenant property has
        // no OWNER_TENANT_ID pinning; honouring it would AND a cross-tenant
        // predicate into the user query. The LIST projection MUST fail closed,
        // mirroring the per-record gate's
        // `scope_without_owner_tenant_filter_is_denied`.
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            usage_record::PROP_RESOURCE_TYPE,
            "compute.vm",
        )]));
        let err = scope_to_odata_filter(&scope).expect_err("tenant-less constraint -> denied");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }

    #[test]
    fn multi_constraint_denied_when_any_disjunct_lacks_tenant_pinning() {
        // Constraints are OR-ed independent access paths; one tenant-less path
        // would still widen the projection cross-tenant, so the whole scope
        // fails closed rather than silently dropping the offending disjunct.
        let t = uid(1);
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t)]),
            ScopeConstraint::new(vec![ScopeFilter::eq(
                usage_record::PROP_RESOURCE_TYPE,
                "compute.vm",
            )]),
        ]);
        let err = scope_to_odata_filter(&scope).expect_err("tenant-less disjunct -> denied");
        assert!(matches!(err, DomainError::AuthorizationDenied { .. }));
    }
}

// ---------------------------------------------------------------------------
// scope_admits_attribution_tuple — per-record attribution gate applied after a
// permit. Verifies the record's *full* attribution tuple (tenant + any other
// narrowing predicate the PDP returned) satisfies the granted scope, not just
// the owning tenant.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod attribution_gate_tests {
    use super::SAMPLE_MEMBER_TYPE;
    use toolkit_security::{
        AccessScope, InGroupScopeFilter, ScopeConstraint, ScopeFilter, ScopeValue, pep_properties,
    };
    use usage_collector_sdk::{ResourceRef, UsageRecord};
    use uuid::Uuid;

    use super::super::{AttributionTupleKey, scope_admits_attribution_tuple, usage_record};

    fn uid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    /// A tuple key whose only meaningful attribute for tenant-only scopes is the
    /// owning tenant (resource attributes come from the shared record builder).
    fn key_for_tenant(tenant: Uuid) -> AttributionTupleKey {
        AttributionTupleKey::from_record(
            &super::record_with_tenant(tenant),
            usage_record::actions::CREATE,
        )
    }

    /// A tuple key with caller-chosen tenant and resource reference, for driving
    /// the non-tenant predicate paths.
    fn key_with_resource(
        tenant: Uuid,
        resource_id: &str,
        resource_type: &str,
    ) -> AttributionTupleKey {
        let record = UsageRecord {
            tenant_id: tenant,
            resource_ref: ResourceRef::new(resource_id, resource_type).expect("valid resource ref"),
            ..super::record_with(None)
        };
        AttributionTupleKey::from_record(&record, usage_record::actions::CREATE)
    }

    #[test]
    fn unconstrained_scope_is_denied_fail_closed() {
        // Under require_constraints(true) a legitimate permit always carries an
        // OWNER_TENANT_ID In[..] narrowing (a Global-scoped admin resolves to
        // In[all tenants], NOT allow_all — covered by the In-closure cases). An
        // unconstrained scope here only arises from a degenerate empty-predicate
        // permit, so the per-record gate MUST fail closed rather than admit
        // every tenant, matching the LIST path's fail-closed handling.
        assert!(!scope_admits_attribution_tuple(
            &AccessScope::allow_all(),
            &key_for_tenant(uid(0xAB))
        ));
    }

    #[test]
    fn deny_all_scope_admits_no_tenant() {
        assert!(!scope_admits_attribution_tuple(
            &AccessScope::deny_all(),
            &key_for_tenant(uid(0xAB))
        ));
    }

    #[test]
    fn tenant_within_in_closure_is_admitted() {
        let a = uid(1);
        let b = uid(2);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
            pep_properties::OWNER_TENANT_ID,
            vec![a, b],
        )]));
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(a)));
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(b)));
    }

    #[test]
    fn tenant_outside_in_closure_is_denied() {
        // The cross-tenant case: caller scoped to {a}, record names some
        // other tenant -> fail closed.
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
            pep_properties::OWNER_TENANT_ID,
            vec![uid(1)],
        )]));
        assert!(!scope_admits_attribution_tuple(
            &scope,
            &key_for_tenant(uid(0xC))
        ));
    }

    #[test]
    fn tenant_as_uuid_string_value_is_admitted() {
        // The compiler may emit a UUID as a String ScopeValue; the gate MUST
        // accept it (mirrors scope_value_to_ast / AccessScope::contains_uuid).
        let t = uid(0x1234);
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::String(t.to_string()),
        )]));
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(t)));
    }

    #[test]
    fn scope_without_owner_tenant_filter_is_denied() {
        // A constrained permit that narrows ONLY by some other property (no
        // OWNER_TENANT_ID filter) MUST NOT admit — the gate requires the
        // record's owning tenant to be pinned by the granted scope, even when
        // the non-tenant predicate happens to match. Fail closed.
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            usage_record::PROP_RESOURCE_TYPE,
            "compute.vm",
        )]));
        assert!(!scope_admits_attribution_tuple(
            &scope,
            &key_with_resource(uid(0xAB), "rsc-eq", "compute.vm")
        ));
    }

    #[test]
    fn multi_constraint_disjunction_admits_when_any_path_covers_tenant() {
        // Constraints are OR-ed (independent access paths); a record tenant
        // covered by ANY path is admitted.
        let a = uid(1);
        let b = uid(2);
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, a)]),
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, b)]),
        ]);
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(a)));
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(b)));
        assert!(!scope_admits_attribution_tuple(
            &scope,
            &key_for_tenant(uid(3))
        ));
    }

    // --- full-tuple gating: non-tenant predicates must constrain (finding #1) ---

    #[test]
    fn constraint_with_satisfied_resource_id_narrowing_is_admitted() {
        // Tenant pinned AND the record's resource_id matches the narrowing the
        // PDP returned -> admit. Guards against over-correction (the gate must
        // not deny a record the PDP actually granted).
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, "granted-rsc"),
        ]));
        assert!(scope_admits_attribution_tuple(
            &scope,
            &key_with_resource(t, "granted-rsc", "compute.vm")
        ));
    }

    #[test]
    fn constraint_with_violated_resource_id_narrowing_is_denied() {
        // THE BUG #1 closes: the permit pins the tenant AND narrows resource_id
        // to a value the record does NOT carry. A tenant-only gate ignores the
        // resource_id filter and grants more than the PDP intended (within
        // tenant); the full-tuple gate MUST honour every filter and deny.
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, "granted-rsc"),
        ]));
        assert!(!scope_admits_attribution_tuple(
            &scope,
            &key_with_resource(t, "other-rsc", "compute.vm")
        ));
    }

    #[test]
    fn constraint_carrying_unknown_property_fails_closed_even_with_tenant_match() {
        // A permit narrowed by a property this gear doesn't understand cannot be
        // evaluated against a flat per-record tuple; as defensive as the LIST
        // path, the gate refuses to admit on the strength of the tenant filter
        // alone.
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq("unsupported_prop", "x"),
        ]));
        assert!(!scope_admits_attribution_tuple(&scope, &key_for_tenant(t)));
    }

    #[test]
    fn constraint_carrying_tree_predicate_fails_closed_even_with_tenant_match() {
        // usage_records is a flat resource with no group/closure membership; a
        // tree predicate alongside the tenant filter is unevaluable here and
        // MUST fail closed (mirrors scope_to_odata_filter's rejection).
        let t = uid(1);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::InGroup(InGroupScopeFilter::new_typed(
                "owner_id",
                SAMPLE_MEMBER_TYPE,
                vec![ScopeValue::Uuid(uid(9))],
            )),
        ]));
        assert!(!scope_admits_attribution_tuple(&scope, &key_for_tenant(t)));
    }

    #[test]
    fn disjunction_admits_via_clean_constraint_despite_unevaluable_sibling() {
        // Constraints are OR-ed. A record covered by a clean tenant constraint
        // is admitted even when a SIBLING constraint carries an unevaluable
        // predicate — dropping the bad disjunct only ever narrows access, never
        // over-grants. (Pins the per-constraint fail-closed choice over a
        // whole-scope hard error.)
        let t = uid(1);
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t)]),
            ScopeConstraint::new(vec![ScopeFilter::InGroup(InGroupScopeFilter::new_typed(
                "owner_id",
                SAMPLE_MEMBER_TYPE,
                vec![ScopeValue::Uuid(uid(9))],
            ))]),
        ]);
        assert!(scope_admits_attribution_tuple(&scope, &key_for_tenant(t)));
    }
}

// ---------------------------------------------------------------------------
// Cross-gate leaf consistency (Architecture finding #1)
//
// The per-record gate (`scope_admits_attribution_tuple`) and the LIST
// projection (`scope_to_odata_filter`) share two leaf decisions: the
// recognized PEP property set and the `ScopeValue` coercion (which value kinds
// each property accepts). They have drifted there before (finding #2). They
// legitimately DIFFER at the policy layer — tenant-must-be-pinned is
// point-gate-only (LIST trusts the PDP constraint to carry the tenant list),
// bad disjuncts drop on the point side but fail the whole scope on the LIST
// side, and only the point gate matches a concrete record's values. So this
// suite pins agreement on the *leaf* verdict alone: a single tenant-pinned
// constraint, evaluated against a record built to carry the canonical value,
// must be accepted-or-rejected the same way by both gates.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod gate_leaf_consistency_tests {
    use toolkit_security::{
        AccessScope, InTenantSubtreeScopeFilter, ScopeConstraint, ScopeFilter, ScopeValue,
        pep_properties,
    };
    use usage_collector_sdk::{ResourceRef, UsageRecord};
    use uuid::Uuid;

    use super::super::{
        AttributionTupleKey, scope_admits_attribution_tuple, scope_to_odata_filter, usage_record,
    };

    fn uid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    /// A key whose attribution tuple carries exactly the given tenant /
    /// `resource_id` / `resource_type`, so a *coercible* filter value also
    /// MATCHES the record. That isolates the structural accept/deny (unknown
    /// property, value typing) from a mere value mismatch.
    fn key(tenant: Uuid, resource_id: &str, resource_type: &str) -> AttributionTupleKey {
        let record = UsageRecord {
            tenant_id: tenant,
            resource_ref: ResourceRef::new(resource_id, resource_type).expect("valid resource ref"),
            ..super::record_with(None)
        };
        AttributionTupleKey::from_record(&record, usage_record::actions::CREATE)
    }

    #[test]
    fn gates_agree_on_leaf_verdict_across_scope_corpus() {
        let t = uid(0x7000);
        let u = uid(0x9999);
        let rid_uuid = u.to_string();

        // (label, single constraint, matching key). Tenant is pinned in every
        // case so the point gate *can* admit; whether it does then turns purely
        // on the shared leaf decisions, which is what we assert agreement on.
        let cases: Vec<(&str, ScopeConstraint, AttributionTupleKey)> = vec![
            (
                "tenant: uuid value",
                ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t)]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "tenant: uuid-as-string value",
                ScopeConstraint::new(vec![ScopeFilter::eq(
                    pep_properties::OWNER_TENANT_ID,
                    ScopeValue::String(t.to_string()),
                )]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "tenant: non-uuid string value (type mismatch)",
                ScopeConstraint::new(vec![ScopeFilter::eq(
                    pep_properties::OWNER_TENANT_ID,
                    ScopeValue::String("not-a-uuid".into()),
                )]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "resource_id: string value",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, "rsc"),
                ]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "resource_id: uuid value on a string field (finding #2 drift)",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::eq(usage_record::PROP_RESOURCE_ID, ScopeValue::Uuid(u)),
                ]),
                key(t, &rid_uuid, "compute.vm"),
            ),
            (
                "resource_type: uuid value on a string field (finding #2 drift)",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::eq(usage_record::PROP_RESOURCE_TYPE, ScopeValue::Uuid(u)),
                ]),
                key(t, "rsc", &rid_uuid),
            ),
            (
                "unknown property",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::eq("unsupported_prop", "x"),
                ]),
                key(t, "rsc", "compute.vm"),
            ),
            (
                "tree predicate alongside tenant",
                ScopeConstraint::new(vec![
                    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
                    ScopeFilter::InTenantSubtree(InTenantSubtreeScopeFilter::new(
                        pep_properties::OWNER_TENANT_ID,
                        t,
                    )),
                ]),
                key(t, "rsc", "compute.vm"),
            ),
        ];

        for (label, constraint, key) in cases {
            let scope = AccessScope::single(constraint);
            let list_accepts = scope_to_odata_filter(&scope).is_ok();
            let point_admits = scope_admits_attribution_tuple(&scope, &key);
            assert_eq!(
                list_accepts, point_admits,
                "LIST projection and per-record gate disagree on the leaf verdict for \
                 `{label}`: list_accepts={list_accepts}, point_admits={point_admits}",
            );
        }
    }
}
