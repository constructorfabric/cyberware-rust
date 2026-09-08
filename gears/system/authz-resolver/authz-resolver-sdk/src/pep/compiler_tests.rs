// Created: 2026-04-14 by Constructor Tech
use super::*;
use crate::constraints::{EqPredicate, InPredicate};
use crate::models::EvaluationResponseContext;
use serde_json::json;
use toolkit_security::pep_properties;
use uuid::Uuid;

fn uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}

/// Helper: UUID string as `serde_json::Value`.
fn jid(s: &str) -> serde_json::Value {
    json!(s)
}

const T1: &str = "11111111-1111-1111-1111-111111111111";
const T2: &str = "22222222-2222-2222-2222-222222222222";
const R1: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

const DEFAULT_PROPS: &[&str] = &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID];

// === Constraint Compilation Matrix Tests ===

#[test]
fn no_require_constraints_empty_returns_allow_all() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext::default(),
    };

    let scope = compile_to_access_scope(&response, false, DEFAULT_PROPS).unwrap();
    assert!(scope.is_unconstrained());
}

#[test]
fn no_require_constraints_with_constraints_compiles_them() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::Eq(EqPredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    value: jid(T1),
                })],
            }],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, false, DEFAULT_PROPS).unwrap();
    assert!(!scope.is_unconstrained());
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(T1)]
    );
}

#[test]
fn decision_true_require_constraints_empty_returns_error() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext::default(),
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(matches!(
        result,
        Err(ConstraintCompileError::ConstraintsRequiredButAbsent)
    ));
}

/// DESIGN.md, PEP requirement #7: a NON-empty constraints list containing a
/// degenerate `predicates: []` constraint is malformed and must fail closed —
/// it must never fold into `allow_all()`, which would turn a degenerate PDP
/// permit into unrestricted access. Pinned for both `require_constraints`
/// modes: the empty-outer-list gate at the top of compilation never sees this
/// shape.
#[test]
fn empty_predicates_constraint_fails_closed_never_allow_all() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint { predicates: vec![] }],
            ..Default::default()
        },
    };

    for require_constraints in [true, false] {
        let result = compile_to_access_scope(&response, require_constraints, DEFAULT_PROPS);
        let Err(ConstraintCompileError::AllConstraintsFailed { reason }) = result else {
            panic!(
                "empty-predicates constraint must fail closed \
                 (require_constraints={require_constraints}): {result:?}"
            );
        };
        assert!(reason.contains("empty predicates"));
    }
}

/// A degenerate empty-predicates constraint alongside a valid one is dropped
/// (constraints are OR-ed alternative grants — losing an unexecutable branch
/// only narrows access), while the valid constraint survives.
#[test]
fn empty_predicates_constraint_dropped_when_sibling_is_valid() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![
                Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        value: jid(T1),
                    })],
                },
                Constraint { predicates: vec![] },
            ],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    assert!(!scope.is_unconstrained());
    assert_eq!(scope.constraints().len(), 1);
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(T1)]
    );
}

// === Constraint Compilation Tests ===

#[test]
fn single_tenant_eq_constraint() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::Eq(EqPredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    value: jid(T1),
                })],
            }],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(T1)]
    );
    assert!(
        scope
            .all_uuid_values_for(pep_properties::RESOURCE_ID)
            .is_empty()
    );

    // Verify Predicate::Eq produces ScopeFilter::Eq (not In)
    let filter = &scope.constraints()[0].filters()[0];
    assert!(matches!(filter, ScopeFilter::Eq(_)));
}

#[test]
fn multiple_tenants_in_constraint() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::In(InPredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    values: vec![jid(T1), jid(T2)],
                })],
            }],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(T1), uuid(T2)]
    );
}

#[test]
fn resource_id_eq_constraint() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::Eq(EqPredicate {
                    property: pep_properties::RESOURCE_ID.to_owned(),
                    value: jid(R1),
                })],
            }],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    assert!(
        scope
            .all_uuid_values_for(pep_properties::OWNER_TENANT_ID)
            .is_empty()
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::RESOURCE_ID),
        &[uuid(R1)]
    );

    // Verify Predicate::Eq produces ScopeFilter::Eq
    let filter = &scope.constraints()[0].filters()[0];
    assert!(matches!(filter, ScopeFilter::Eq(_)));
}

#[test]
fn multiple_constraints_produce_or_scope() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![
                Constraint {
                    predicates: vec![Predicate::In(InPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        values: vec![jid(T1)],
                    })],
                },
                Constraint {
                    predicates: vec![Predicate::In(InPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        values: vec![jid(T2)],
                    })],
                },
            ],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    // Each constraint is a separate ScopeConstraint (ORed)
    assert_eq!(scope.constraints().len(), 2);
    // Both tenants accessible
    assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uuid(T1)));
    assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uuid(T2)));
}

#[test]
fn unknown_predicate_fails_constraint() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::Eq(EqPredicate {
                    property: "unknown_property".to_owned(),
                    value: jid(T1),
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(matches!(
        result,
        Err(ConstraintCompileError::AllConstraintsFailed { .. })
    ));
}

#[test]
fn mixed_known_and_unknown_constraints() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![
                // This constraint has an unknown property → fails
                Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate {
                        property: "group_id".to_owned(),
                        value: jid(T1),
                    })],
                },
                // This constraint is valid → succeeds
                Constraint {
                    predicates: vec![Predicate::In(InPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        values: vec![jid(T2)],
                    })],
                },
            ],
            ..Default::default()
        },
    };

    // Should succeed - the second constraint compiled
    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(T2)]
    );
}

#[test]
fn both_tenant_and_resource_in_single_constraint() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        values: vec![jid(T1)],
                    }),
                    Predicate::Eq(EqPredicate {
                        property: pep_properties::RESOURCE_ID.to_owned(),
                        value: jid(R1),
                    }),
                ],
            }],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    // Single constraint with both properties (AND)
    assert_eq!(scope.constraints().len(), 1);
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(T1)]
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::RESOURCE_ID),
        &[uuid(R1)]
    );
}

#[test]
fn mixed_shape_constraints_produce_or_scope() {
    // T1+R1 (AND) OR T2 - two different-shaped constraints
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![
                Constraint {
                    predicates: vec![
                        Predicate::In(InPredicate {
                            property: pep_properties::OWNER_TENANT_ID.to_owned(),
                            values: vec![jid(T1)],
                        }),
                        Predicate::Eq(EqPredicate {
                            property: pep_properties::RESOURCE_ID.to_owned(),
                            value: jid(R1),
                        }),
                    ],
                },
                Constraint {
                    predicates: vec![Predicate::In(InPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        values: vec![jid(T2)],
                    })],
                },
            ],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    assert_eq!(scope.constraints().len(), 2);
    // First constraint has 2 filters (AND), second has 1 filter
    assert_eq!(scope.constraints()[0].filters().len(), 2);
    assert_eq!(scope.constraints()[1].filters().len(), 1);
}

// === InGroup / InGroupSubtree Compilation Tests ===

const GROUP_MEMBERSHIP_TYPE: &str = "gts.cf.core.rg.type.v1~example.core.rg.member.v1~";

fn compile_group_scope(
    response: &EvaluationResponse,
) -> Result<AccessScope, ConstraintCompileError> {
    compile_to_access_scope_with_group_membership_type(
        response,
        true,
        DEFAULT_PROPS,
        Some(GROUP_MEMBERSHIP_TYPE),
    )
}

#[test]
fn in_group_predicate_compiles_to_type_qualified_in_group_filter() {
    use crate::constraints::InGroupPredicate;

    let g1 = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        values: vec![jid(T1)],
                    }),
                    Predicate::InGroup(InGroupPredicate {
                        property: pep_properties::RESOURCE_ID.to_owned(),
                        group_ids: vec![jid(g1)],
                    }),
                ],
            }],
            ..Default::default()
        },
    };

    let scope = compile_group_scope(&response).unwrap();
    assert_eq!(scope.constraints().len(), 1);
    let filter = &scope.constraints()[0].filters()[1];
    assert_eq!(filter.property(), pep_properties::RESOURCE_ID);
    let ScopeFilter::InGroup(filter) = filter else {
        panic!("expected InGroup filter, got: {filter:?}");
    };
    assert_eq!(filter.membership_resource_type(), GROUP_MEMBERSHIP_TYPE);
}

#[test]
fn in_group_subtree_predicate_compiles_to_type_qualified_subtree_filter() {
    use crate::constraints::InGroupSubtreePredicate;

    let ancestor = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        values: vec![jid(T1)],
                    }),
                    Predicate::InGroupSubtree(InGroupSubtreePredicate {
                        property: pep_properties::RESOURCE_ID.to_owned(),
                        ancestor_ids: vec![jid(ancestor)],
                    }),
                ],
            }],
            ..Default::default()
        },
    };

    let scope = compile_group_scope(&response).unwrap();
    assert_eq!(scope.constraints().len(), 1);
    let filter = &scope.constraints()[0].filters()[1];
    let ScopeFilter::InGroupSubtree(filter) = filter else {
        panic!("expected InGroupSubtree filter, got: {filter:?}");
    };
    assert_eq!(filter.membership_resource_type(), GROUP_MEMBERSHIP_TYPE);
}

#[test]
fn native_group_predicate_without_membership_type_fails_closed() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        values: vec![jid(T1)],
                    }),
                    Predicate::InGroup(InGroupPredicate::new(
                        pep_properties::RESOURCE_ID,
                        [uuid(R1)],
                    )),
                ],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    let Err(ConstraintCompileError::AllConstraintsFailed { reason }) = result else {
        panic!("untyped native group predicate must fail closed: {result:?}");
    };
    assert!(reason.contains("requires a configured RG membership resource type"));
}

#[test]
fn group_predicate_without_tenant_in_same_constraint_fails_closed() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InGroup(InGroupPredicate::new(
                    pep_properties::RESOURCE_ID,
                    [uuid(R1)],
                ))],
            }],
            ..Default::default()
        },
    };

    let result = compile_group_scope(&response);
    let Err(ConstraintCompileError::AllConstraintsFailed { reason }) = result else {
        panic!("group-only constraint must fail closed: {result:?}");
    };
    assert!(reason.contains("owner_tenant_id predicate in the same constraint"));
}

#[test]
fn malformed_group_uuid_fails_before_sql_generation() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [uuid(T1)],
                    )),
                    Predicate::InGroup(InGroupPredicate {
                        property: pep_properties::RESOURCE_ID.to_owned(),
                        group_ids: vec![json!("not-a-uuid")],
                    }),
                ],
            }],
            ..Default::default()
        },
    };

    let result = compile_group_scope(&response);
    let Err(ConstraintCompileError::AllConstraintsFailed { reason }) = result else {
        panic!("malformed group UUID must fail closed: {result:?}");
    };
    assert!(reason.contains("group_ids must contain UUID strings"));
}

#[test]
fn malformed_tenant_predicate_cannot_satisfy_group_tenant_guard() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::Eq(EqPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        value: json!(false),
                    }),
                    Predicate::InGroup(InGroupPredicate::new(
                        pep_properties::RESOURCE_ID,
                        [uuid(R1)],
                    )),
                ],
            }],
            ..Default::default()
        },
    };

    let result = compile_group_scope(&response);
    let Err(ConstraintCompileError::AllConstraintsFailed { reason }) = result else {
        panic!("malformed tenant scope must fail closed: {result:?}");
    };
    assert!(reason.contains("owner_tenant_id must contain UUID strings"));
}

#[test]
fn group_predicate_on_non_resource_property_fails_closed() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [uuid(T1)],
                    )),
                    Predicate::InGroup(InGroupPredicate::new(pep_properties::OWNER_ID, [uuid(R1)])),
                ],
            }],
            ..Default::default()
        },
    };
    let supported = &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::RESOURCE_ID,
        pep_properties::OWNER_ID,
    ];

    let result = compile_to_access_scope_with_group_membership_type(
        &response,
        true,
        supported,
        Some(GROUP_MEMBERSHIP_TYPE),
    );
    let Err(ConstraintCompileError::AllConstraintsFailed { reason }) = result else {
        panic!("group predicate on owner_id must fail closed: {result:?}");
    };
    assert!(reason.contains("must target 'id'"));
}

#[test]
fn unadvertised_native_group_predicate_fails_closed() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [uuid(T1)],
                    )),
                    Predicate::InGroup(InGroupPredicate::new(
                        pep_properties::RESOURCE_ID,
                        [uuid(R1)],
                    )),
                ],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope_with_negotiated_capabilities(
        &response,
        true,
        DEFAULT_PROPS,
        Some(GROUP_MEMBERSHIP_TYPE),
        &[Capability::TenantHierarchy],
    );
    let Err(ConstraintCompileError::UnadvertisedCapabilities { predicate, missing }) = result
    else {
        panic!("unadvertised group predicate must fail closed: {result:?}");
    };
    assert_eq!(predicate, "InGroup");
    assert_eq!(missing, vec!["group_membership"]);
}

#[test]
fn unadvertised_tenant_subtree_predicate_fails_closed() {
    use crate::constraints::InTenantSubtreePredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    root_tenant_id: jid(T1),
                    barrier_mode: BarrierMode::Respect,
                    descendant_status: Vec::new(),
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope_with_negotiated_capabilities(
        &response,
        true,
        DEFAULT_PROPS,
        None,
        &[],
    );
    let Err(ConstraintCompileError::UnadvertisedCapabilities { predicate, missing }) = result
    else {
        panic!("unadvertised tenant subtree must fail closed: {result:?}");
    };
    assert_eq!(predicate, "InTenantSubtree");
    assert_eq!(missing, vec!["tenant_hierarchy"]);
}

/// A capability-negotiation failure mixed with a structural failure must
/// aggregate to the generic `AllConstraintsFailed` (joined reasons), not the
/// typed variant — the typed error is reserved for the case where every
/// failed constraint is a negotiation violation.
#[test]
fn mixed_unadvertised_and_structural_failures_aggregate_generic() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![
                // Fails on capability negotiation.
                Constraint {
                    predicates: vec![
                        Predicate::In(InPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            [uuid(T1)],
                        )),
                        Predicate::InGroup(InGroupPredicate::new(
                            pep_properties::RESOURCE_ID,
                            [uuid(R1)],
                        )),
                    ],
                },
                // Fails structurally (unsupported property).
                Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate::new(
                        "no_such_property",
                        uuid(T1),
                    ))],
                },
            ],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope_with_negotiated_capabilities(
        &response,
        true,
        DEFAULT_PROPS,
        Some(GROUP_MEMBERSHIP_TYPE),
        &[Capability::TenantHierarchy],
    );
    let Err(ConstraintCompileError::AllConstraintsFailed { reason }) = result else {
        panic!("mixed failures must aggregate to AllConstraintsFailed: {result:?}");
    };
    assert!(reason.contains("unadvertised capabilities: group_membership"));
    assert!(reason.contains("unsupported property: no_such_property"));
}

/// `InGroupSubtree` requires both group capabilities; with only
/// `GroupMembership` negotiated the typed error must name the one missing
/// capability, and with neither negotiated it must render both, exercising
/// the multi-element `missing` list.
#[test]
fn in_group_subtree_reports_missing_capability_set() {
    use crate::constraints::InGroupSubtreePredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [uuid(T1)],
                    )),
                    Predicate::InGroupSubtree(InGroupSubtreePredicate::new(
                        pep_properties::RESOURCE_ID,
                        [uuid(R1)],
                    )),
                ],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope_with_negotiated_capabilities(
        &response,
        true,
        DEFAULT_PROPS,
        Some(GROUP_MEMBERSHIP_TYPE),
        &[Capability::GroupMembership],
    );
    let Err(ConstraintCompileError::UnadvertisedCapabilities { predicate, missing }) = result
    else {
        panic!("subtree without GroupHierarchy must fail closed: {result:?}");
    };
    assert_eq!(predicate, "InGroupSubtree");
    assert_eq!(missing, vec!["group_hierarchy"]);

    let result = compile_to_access_scope_with_negotiated_capabilities(
        &response,
        true,
        DEFAULT_PROPS,
        Some(GROUP_MEMBERSHIP_TYPE),
        &[],
    );
    let Err(error @ ConstraintCompileError::UnadvertisedCapabilities { .. }) = result else {
        panic!("subtree without either capability must fail closed: {result:?}");
    };
    assert_eq!(
        error.to_string(),
        "InGroupSubtree predicate requires unadvertised capabilities: \
         group_membership, group_hierarchy (fail-closed)"
    );
}

#[test]
fn tenant_and_group_in_separate_or_branches_drops_unsafe_group_branch() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![
                Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [uuid(T1)],
                    ))],
                },
                Constraint {
                    predicates: vec![Predicate::InGroup(InGroupPredicate::new(
                        pep_properties::RESOURCE_ID,
                        [uuid(R1)],
                    ))],
                },
            ],
            ..Default::default()
        },
    };

    let scope = compile_group_scope(&response).expect("safe tenant branch should remain");
    assert_eq!(scope.constraints().len(), 1);
    assert_eq!(scope.constraints()[0].filters().len(), 1);
    assert_eq!(
        scope.constraints()[0].filters()[0].property(),
        pep_properties::OWNER_TENANT_ID
    );
}

#[test]
fn tenant_plus_in_group_in_single_constraint() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate {
                        property: pep_properties::OWNER_TENANT_ID.to_owned(),
                        values: vec![jid(T1)],
                    }),
                    Predicate::InGroup(InGroupPredicate {
                        property: pep_properties::RESOURCE_ID.to_owned(),
                        group_ids: vec![jid(R1)],
                    }),
                ],
            }],
            ..Default::default()
        },
    };

    let scope = compile_group_scope(&response).unwrap();
    assert_eq!(scope.constraints().len(), 1);
    // Constraint should have 2 filters: In(tenant) AND InGroup(resource)
    assert_eq!(scope.constraints()[0].filters().len(), 2);
}

#[test]
fn supported_properties_validation() {
    // Only owner_tenant_id is supported - id should fail
    let limited_props: &[&str] = &[pep_properties::OWNER_TENANT_ID];

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::Eq(EqPredicate {
                    property: pep_properties::RESOURCE_ID.to_owned(),
                    value: jid(R1),
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, limited_props);
    assert!(matches!(
        result,
        Err(ConstraintCompileError::AllConstraintsFailed { .. })
    ));
}

// === Empty value list guards (fail-closed) ===

#[test]
fn empty_in_values_fails_constraint() {
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::In(InPredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    values: vec![],
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(
        matches!(
            result,
            Err(ConstraintCompileError::AllConstraintsFailed { .. })
        ),
        "empty In values must fail-closed, got: {result:?}"
    );
}

#[test]
fn empty_in_group_ids_fails_constraint() {
    use crate::constraints::InGroupPredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InGroup(InGroupPredicate {
                    property: pep_properties::RESOURCE_ID.to_owned(),
                    group_ids: vec![],
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(
        matches!(
            result,
            Err(ConstraintCompileError::AllConstraintsFailed { .. })
        ),
        "empty InGroup group_ids must fail-closed, got: {result:?}"
    );
}

#[test]
fn in_tenant_subtree_predicate_compiles_to_subtree_filter_respecting_barriers() {
    use crate::constraints::InTenantSubtreePredicate;
    use crate::models::BarrierMode;

    let root = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    root_tenant_id: jid(root),
                    barrier_mode: BarrierMode::Respect,
                    descendant_status: Vec::new(),
                })],
            }],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    assert_eq!(scope.constraints().len(), 1);
    let filter = &scope.constraints()[0].filters()[0];
    match filter {
        ScopeFilter::InTenantSubtree(sf) => {
            assert_eq!(sf.property(), pep_properties::OWNER_TENANT_ID);
            assert!(sf.respect_barriers(), "Respect must clamp on barrier");
        }
        other => panic!("expected InTenantSubtree filter, got: {other:?}"),
    }
}

#[test]
fn in_tenant_subtree_predicate_ignore_barriers_propagates_to_filter() {
    use crate::constraints::InTenantSubtreePredicate;
    use crate::models::BarrierMode;

    let root = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    root_tenant_id: jid(root),
                    barrier_mode: BarrierMode::Ignore,
                    descendant_status: Vec::new(),
                })],
            }],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    let filter = &scope.constraints()[0].filters()[0];
    match filter {
        ScopeFilter::InTenantSubtree(sf) => {
            assert!(!sf.respect_barriers(), "Ignore must not clamp on barrier");
        }
        other => panic!("expected InTenantSubtree filter, got: {other:?}"),
    }
}

#[test]
fn in_tenant_subtree_with_descendant_status_compiles_to_smallint_filter() {
    // Non-empty descendant_status must flow through the compiler unchanged
    // in semantics — each TenantStatus lowers to its canonical SMALLINT
    // encoding (Active=1, Suspended=2, Deleted=3) on the ScopeFilter so the
    // SQL bind matches the `tenant_closure.descendant_status` domain.
    use crate::constraints::InTenantSubtreePredicate;
    use crate::models::BarrierMode;
    use tenant_resolver_sdk::TenantStatus;
    use toolkit_security::ScopeValue;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    root_tenant_id: jid(T1),
                    barrier_mode: BarrierMode::Respect,
                    descendant_status: vec![TenantStatus::Active, TenantStatus::Suspended],
                })],
            }],
            ..Default::default()
        },
    };

    let scope = compile_to_access_scope(&response, true, DEFAULT_PROPS).unwrap();
    let filter = &scope.constraints()[0].filters()[0];
    match filter {
        ScopeFilter::InTenantSubtree(sf) => {
            assert_eq!(
                sf.descendant_status(),
                &[ScopeValue::Int(1), ScopeValue::Int(2)],
                "TenantStatus must lower to as_smallint() values"
            );
            assert!(sf.respect_barriers());
        }
        other => panic!("expected InTenantSubtree filter, got: {other:?}"),
    }
}

#[test]
fn in_tenant_subtree_non_uuid_root_bool_fails_constraint() {
    use crate::constraints::InTenantSubtreePredicate;
    use crate::models::BarrierMode;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    root_tenant_id: serde_json::json!(true),
                    barrier_mode: BarrierMode::Respect,
                    descendant_status: Vec::new(),
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(
        matches!(
            result,
            Err(ConstraintCompileError::AllConstraintsFailed { .. })
        ),
        "bool root_tenant_id must fail-closed, got: {result:?}"
    );
}

#[test]
fn in_tenant_subtree_non_uuid_root_int_fails_constraint() {
    use crate::constraints::InTenantSubtreePredicate;
    use crate::models::BarrierMode;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    root_tenant_id: serde_json::json!(42),
                    barrier_mode: BarrierMode::Respect,
                    descendant_status: Vec::new(),
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(
        matches!(
            result,
            Err(ConstraintCompileError::AllConstraintsFailed { .. })
        ),
        "integer root_tenant_id must fail-closed, got: {result:?}"
    );
}

#[test]
fn in_tenant_subtree_non_uuid_root_string_fails_constraint() {
    use crate::constraints::InTenantSubtreePredicate;
    use crate::models::BarrierMode;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    root_tenant_id: serde_json::json!("not-a-uuid"),
                    barrier_mode: BarrierMode::Respect,
                    descendant_status: Vec::new(),
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(
        matches!(
            result,
            Err(ConstraintCompileError::AllConstraintsFailed { .. })
        ),
        "non-UUID string root_tenant_id must fail-closed, got: {result:?}"
    );
}

#[test]
fn in_tenant_subtree_non_uuid_root_null_fails_constraint() {
    use crate::constraints::InTenantSubtreePredicate;
    use crate::models::BarrierMode;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    root_tenant_id: serde_json::Value::Null,
                    barrier_mode: BarrierMode::Respect,
                    descendant_status: Vec::new(),
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(
        matches!(
            result,
            Err(ConstraintCompileError::AllConstraintsFailed { .. })
        ),
        "null root_tenant_id must fail-closed, got: {result:?}"
    );
}

#[test]
fn in_tenant_subtree_non_uuid_root_array_fails_constraint() {
    use crate::constraints::InTenantSubtreePredicate;
    use crate::models::BarrierMode;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate {
                    property: pep_properties::OWNER_TENANT_ID.to_owned(),
                    root_tenant_id: serde_json::json!([T1]),
                    barrier_mode: BarrierMode::Respect,
                    descendant_status: Vec::new(),
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(
        matches!(
            result,
            Err(ConstraintCompileError::AllConstraintsFailed { .. })
        ),
        "array root_tenant_id must fail-closed, got: {result:?}"
    );
}

#[test]
fn empty_in_group_subtree_ancestor_ids_fails_constraint() {
    use crate::constraints::InGroupSubtreePredicate;

    let response = EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::InGroupSubtree(InGroupSubtreePredicate {
                    property: pep_properties::RESOURCE_ID.to_owned(),
                    ancestor_ids: vec![],
                })],
            }],
            ..Default::default()
        },
    };

    let result = compile_to_access_scope(&response, true, DEFAULT_PROPS);
    assert!(
        matches!(
            result,
            Err(ConstraintCompileError::AllConstraintsFailed { .. })
        ),
        "empty InGroupSubtree ancestor_ids must fail-closed, got: {result:?}"
    );
}
