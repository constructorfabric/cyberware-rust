#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::field_reassign_with_default,
    clippy::items_after_statements,
    clippy::inconsistent_struct_constructor
)]

use super::*;
use std::collections::HashMap;

use crate::config::{AuthZResolverPluginConfig, MUTATING_BOUNDARY_VERBS, ScopeEnforcementConfig};
use crate::domain::deny::error_codes::SCOPE_MISMATCH_V1;

fn default_config() -> Arc<AuthZResolverPluginConfig> {
    Arc::new(AuthZResolverPluginConfig {
        vendor: "cf".to_owned(),
        ..AuthZResolverPluginConfig::default()
    })
}

fn enforcer() -> ScopeEnforcer {
    ScopeEnforcer::new(default_config())
}

fn assert_deny(result: Result<(), EvaluationResponse>) -> EvaluationResponse {
    match result {
        Err(response) => {
            assert!(!response.decision);
            let reason = response
                .context
                .deny_reason
                .as_ref()
                .expect("deny_reason must be populated");
            assert_eq!(reason.error_code, SCOPE_MISMATCH_V1);
            response
        }
        Ok(()) => panic!("expected deny, got Ok"),
    }
}

fn s(value: &str) -> String {
    value.to_owned()
}

#[test]
fn u_35_wildcard_short_circuits_without_operation_lookup() {
    // Use an action not in the default map so a stray lookup would deny —
    // wildcard must short-circuit before reaching that path.
    let result = enforcer().check_scopes(&[s("*")], "some_action_not_in_map");
    assert!(result.is_ok(), "wildcard must pass: {result:?}");
}

#[test]
fn u_36_empty_scopes_deny() {
    assert_deny(enforcer().check_scopes(&[], "read"));
}

#[test]
fn u_37_matching_namespaced_scope_passes() {
    let result = enforcer().check_scopes(&[s("read:events")], "read");
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn u_38_class_mismatch_denies() {
    let response = assert_deny(enforcer().check_scopes(&[s("read:events")], "delete"));
    let details = response
        .context
        .deny_reason
        .and_then(|r| r.details)
        .expect("details should be populated");
    assert!(
        details.contains("delete") && details.contains("write"),
        "details should name the operation and required class: {details}"
    );
}

#[test]
fn u_39_unmapped_operation_falls_back_to_default_unmapped_scope() {
    // Default unmapped scope is "write"; no token matches "write".
    assert_deny(enforcer().check_scopes(&[s("read:events")], "an_unmapped_op"));
}

#[test]
fn exact_equality_matches() {
    let result = enforcer().check_scopes(&[s("read")], "read");
    assert!(result.is_ok());
}

#[test]
fn unbounded_prefix_does_not_match() {
    // `"readonly"` starts with `"read"` but not with `"read:"`.
    assert_deny(enforcer().check_scopes(&[s("readonly")], "read"));
}

#[test]
fn sub_scope_satisfies_class_via_colon_prefix() {
    // `"read:*"` starts with `"read:"`, so it satisfies scope class
    // `"read"` via the colon-prefix rule. The PDP does NOT interpret the
    // trailing `*` — anything after the colon is opaque sub-scope text,
    // not a wildcard.
    let result = enforcer().check_scopes(&[s("read:*")], "read");
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn custom_default_unmapped_scope_honored() {
    let mut scope_cfg = ScopeEnforcementConfig::default();
    scope_cfg.default_unmapped_scope = "read".to_owned();
    let config = Arc::new(AuthZResolverPluginConfig {
        vendor: "cf".to_owned(),
        scope_enforcement: scope_cfg,
        ..AuthZResolverPluginConfig::default()
    });

    let result = ScopeEnforcer::new(config).check_scopes(&[s("read")], "unknown_op");
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn custom_wildcard_scope_honored() {
    let mut scope_cfg = ScopeEnforcementConfig::default();
    scope_cfg.wildcard_scope = "ALL".to_owned();
    let config = Arc::new(AuthZResolverPluginConfig {
        vendor: "cf".to_owned(),
        scope_enforcement: scope_cfg,
        ..AuthZResolverPluginConfig::default()
    });

    let result = ScopeEnforcer::new(config).check_scopes(&[s("ALL")], "delete");
    assert!(result.is_ok(), "{result:?}");

    // Under this config the literal "*" is not the wildcard.
    let result = ScopeEnforcer::new(Arc::new(AuthZResolverPluginConfig {
        vendor: "cf".to_owned(),
        scope_enforcement: {
            let mut c = ScopeEnforcementConfig::default();
            c.wildcard_scope = "ALL".to_owned();
            c
        },
        ..AuthZResolverPluginConfig::default()
    }))
    .check_scopes(&[s("*")], "delete");
    // "*" is neither "ALL" nor "write" nor "write:..." → deny.
    assert_deny(result);
}

#[test]
fn wildcard_among_other_scopes_passes() {
    let result = enforcer().check_scopes(&[s("read:events"), s("*")], "delete");
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn empty_scopes_deny_precedes_wildcard_config() {
    // Even with the standard wildcard config, an empty Vec denies — the
    // empty-check runs before wildcard search.
    assert_deny(enforcer().check_scopes(&[], "read"));
}

#[test]
fn full_default_operation_to_scope_map_behavior() {
    let e = enforcer();

    // `read` → class `read`, matched by `"read:events"`.
    assert!(e.check_scopes(&[s("read:events")], "read").is_ok());

    // `write` / `delete` / `start` / `stop` / `restart` → class `write`.
    for op in ["write", "delete", "start", "stop", "restart"] {
        assert!(
            e.check_scopes(&[s("write:resources")], op).is_ok(),
            "operation '{op}' should map to 'write' class"
        );
    }

    // Read-only token cannot do write operations.
    for op in ["write", "delete", "start", "stop", "restart"] {
        assert_deny(e.check_scopes(&[s("read:events")], op));
    }
}

#[test]
fn fully_custom_operation_to_scope_map_replaces_default() {
    let mut scope_cfg = ScopeEnforcementConfig::default();
    // Operator config: "list" maps to "read", everything else falls back.
    scope_cfg.operation_to_scope = {
        let mut m = HashMap::new();
        m.insert("list".to_owned(), "read".to_owned());
        m
    };
    let config = Arc::new(AuthZResolverPluginConfig {
        vendor: "cf".to_owned(),
        scope_enforcement: scope_cfg,
        ..AuthZResolverPluginConfig::default()
    });
    let e = ScopeEnforcer::new(config);

    // "list" → "read" class via custom map → matches "read:events".
    assert!(e.check_scopes(&[s("read:events")], "list").is_ok());

    // "read" is unmapped here → falls back to default_unmapped_scope
    // ("write"). A `read:events` token cannot satisfy "write".
    assert_deny(e.check_scopes(&[s("read:events")], "read"));
}

#[test]
fn read_only_adapter_operation_derives_the_read_class() {
    // The §3.4 case: an adapter-declared, resource-less data-plane operation id.
    // It is in no map (it cannot be — adapters mint their own ids), so before
    // the boundary-verb derivation it fell back to "write" and a read-scope
    // caller was denied a read-only operation before RBAC was consulted.
    let e = enforcer();
    for op in ["list-buckets", "list_access_keys", "get_adapter_usage"] {
        assert!(
            e.check_scopes(&[s("read:events")], op).is_ok(),
            "read-only adapter-level operation '{op}' must resolve to the read class"
        );
    }
}

#[test]
fn mutating_adapter_operation_keeps_the_write_class() {
    // Boundary verb is a write-class verb, so the derivation agrees with the
    // fallback and a read-scope caller is still denied.
    let e = enforcer();
    for op in ["binding-delete", "delete-access-key", "rotate-keys"] {
        assert_deny(e.check_scopes(&[s("read:events")], op));
        assert!(
            e.check_scopes(&[s("write:adapters")], op).is_ok(),
            "'{op}' must be reachable with a write-class token"
        );
    }
}

#[test]
fn interior_segment_cannot_relax_the_class() {
    // `list` sits in the middle, so it is ignored: neither boundary is a known
    // verb and the id keeps `default_unmapped_scope`. Guards the case a naive
    // "any segment" rule would get wrong — a destructive operation talked down
    // to `read` by a word inside its own name.
    assert_deny(enforcer().check_scopes(&[s("read:events")], "snapshot-list-rollback"));
}

#[test]
fn contradictory_boundary_verbs_fall_back_rather_than_guess() {
    // `read` (read class) at the head, `delete` (write class) at the tail. The
    // derivation refuses to pick a winner and the fallback ("write") applies.
    assert_deny(enforcer().check_scopes(&[s("read:events")], "read-objects-delete"));
}

#[test]
fn explicit_map_entry_wins_over_the_derived_class() {
    // An operator pinning a specific id is the override path for a
    // misleadingly-named operation, so the verbatim lookup must be consulted
    // first: `list-buckets` would derive `read`, but the pin says otherwise.
    let mut scope_cfg = ScopeEnforcementConfig::default();
    scope_cfg
        .operation_to_scope
        .insert("list-buckets".to_owned(), "write".to_owned());
    let e = ScopeEnforcer::new(Arc::new(AuthZResolverPluginConfig {
        vendor: "vz".to_owned(),
        scope_enforcement: scope_cfg,
        ..AuthZResolverPluginConfig::default()
    }));

    assert_deny(e.check_scopes(&[s("read:events")], "list-buckets"));
    assert!(
        e.check_scopes(&[s("write:buckets")], "list-buckets")
            .is_ok()
    );
}

#[test]
fn derived_classes_come_from_the_operator_map() {
    // Every class the derivation can produce comes from the configurable map, so
    // an operator who introduces a domain verb gets it applied to compound ids
    // too — and one who removes a READ verb loses the derivation with it, falling
    // back to `default_unmapped_scope`.
    //
    // Only the mutating boundary verbs are recognized outside the map, and they
    // produce no class of their own: they can force a refusal but can never name
    // the class an id ends up with. So the map remains the sole source of derived
    // classes even though it is not the sole source of recognition.
    let mut scope_cfg = ScopeEnforcementConfig::default();
    scope_cfg.operation_to_scope = {
        let mut m = HashMap::new();
        m.insert("inspect".to_owned(), "read".to_owned());
        m
    };
    let e = ScopeEnforcer::new(Arc::new(AuthZResolverPluginConfig {
        vendor: "vz".to_owned(),
        scope_enforcement: scope_cfg,
        ..AuthZResolverPluginConfig::default()
    }));

    assert!(
        e.check_scopes(&[s("read:events")], "inspect-bucket")
            .is_ok()
    );
    // `list` is not a known verb under this map.
    assert_deny(e.check_scopes(&[s("read:events")], "list-buckets"));
}

#[test]
// One assertion per id shape the rule has to classify: the assertion count IS
// the coverage, and the complexity cap is inflated by it rather than by any
// branching.
#[allow(clippy::cognitive_complexity)]
fn derive_scope_class_rules() {
    let map = ScopeEnforcementConfig::default().operation_to_scope;

    // Verb at either boundary. Hyphenated spellings are kept deliberately: the
    // rule accepts `-` and `_` alike, and a third-party adapter may declare
    // either, so these pin the hyphen half of the separator set. The snake_case
    // half is asserted below.
    assert_eq!(derive_scope_class(&map, "list-objects"), Some("read"));
    assert_eq!(derive_scope_class(&map, "policy-read"), Some("read"));
    assert_eq!(derive_scope_class(&map, "signed-url-write"), Some("write"));
    assert_eq!(derive_scope_class(&map, "policy-delete"), Some("write"));
    // Underscores separate segments too — id style varies across adapters.
    assert_eq!(derive_scope_class(&map, "list_access_keys"), Some("read"));
    // Data-plane ids in the snake_case spelling adapter manifests declare.
    // Renaming an id moves which segments this rule sees, so the class each one
    // resolves to is pinned here — a class flip would deny a read-only caller a
    // read, or admit a write.
    assert_eq!(derive_scope_class(&map, "list_objects"), Some("read"));
    assert_eq!(
        derive_scope_class(&map, "list_object_versions"),
        Some("read")
    );
    assert_eq!(derive_scope_class(&map, "policy_read"), Some("read"));
    assert_eq!(derive_scope_class(&map, "cors_read"), Some("read"));
    assert_eq!(derive_scope_class(&map, "versioning_read"), Some("read"));
    assert_eq!(derive_scope_class(&map, "signed_url_read"), Some("read"));
    assert_eq!(derive_scope_class(&map, "policy_write"), Some("write"));
    assert_eq!(derive_scope_class(&map, "policy_delete"), Some("write"));
    assert_eq!(derive_scope_class(&map, "cors_write"), Some("write"));
    assert_eq!(derive_scope_class(&map, "cors_delete"), Some("write"));
    assert_eq!(derive_scope_class(&map, "versioning_write"), Some("write"));
    assert_eq!(derive_scope_class(&map, "signed_url_write"), Some("write"));
    // Contradiction, and no match at all.
    assert_eq!(derive_scope_class(&map, "read_objects_delete"), None);
    // An interior verb is still ignored — the class here comes from the `rollback`
    // boundary, not from the `list` in the middle, which is why the id resolves to
    // `write` rather than being talked down to `read`.
    assert_eq!(
        derive_scope_class(&map, "snapshot_list_rollback"),
        Some("write")
    );
    // `cors_test` and `head_object` have no boundary verb in the map, so they
    // keep `default_unmapped_scope` — unchanged by the rename.
    assert_eq!(derive_scope_class(&map, "cors_test"), None);
    assert_eq!(derive_scope_class(&map, "head_object"), None);
    // Single-segment ids derive nothing: the verbatim lookup already ran.
    assert_eq!(derive_scope_class(&map, "usage"), None);
    assert_eq!(derive_scope_class(&map, ""), None);
}

/// The one-sided-recognition case, which is only sound while the map knows the
/// mutating boundary verbs.
///
/// A read verb at one boundary derives its class unopposed when the other boundary
/// is unrecognized — the same shape as the legitimate `list_access_keys`, and
/// indistinguishable from it by position. So an id whose real effect sits at the
/// far boundary must be refused by the map recognizing that verb and contradicting
/// the read, which is the only reason the default map carries `create`, `purge` and
/// their neighbours: nothing looks them up whole.
#[test]
fn a_mutating_boundary_verb_is_not_overridden_by_a_read_verb_at_the_other_end() {
    let map = ScopeEnforcementConfig::default().operation_to_scope;

    // Refused (→ `default_unmapped_scope`, i.e. `write`) rather than derived read.
    for op in [
        "read_replica_create",
        "list_objects_purge",
        "get_bucket_remove",
        "list_keys_revoke",
        "read_config_update",
        "get_schema_patch",
    ] {
        assert_eq!(derive_scope_class(&map, op), None, "id '{op}'");
    }

    // And the genuinely read-only ids of the same shape keep deriving `read`.
    for op in [
        "list_access_keys",
        "list_object_versions",
        "get_adapter_usage",
    ] {
        assert_eq!(derive_scope_class(&map, op), Some("read"), "id '{op}'");
    }
}

#[test]
fn scope_class_matches_rules() {
    assert!(scope_class_matches("read", "read"));
    assert!(scope_class_matches("read", "read:events"));
    assert!(scope_class_matches("read", "read:*"));
    assert!(!scope_class_matches("read", "reader"));
    assert!(!scope_class_matches("read", "readonly"));
    assert!(!scope_class_matches("read", "read_only"));
    assert!(!scope_class_matches("read", "write"));
    assert!(!scope_class_matches("read", ""));
}

/// A write-only third-party token is DENIED the read-derived data-plane ids.
///
/// This is the deliberate half of the derivation nobody had pinned. Before the
/// boundary derivation existed these ids were unmapped, fell back to
/// `default_unmapped_scope` (`write`), and a `write:adapters` token satisfied
/// them. Deriving `read` makes the scope class exact in BOTH directions, so the
/// same token is now refused — a write-only grant is not a licence to read.
/// Pinned as intended behaviour, not as an accident: if this ever has to change,
/// it should change here, visibly, and not by a verb quietly leaving the map.
#[test]
fn a_write_only_token_does_not_satisfy_a_read_derived_operation() {
    let map = ScopeEnforcementConfig::default().operation_to_scope;

    // Adapter ids that derive `read` from a boundary verb.
    for op in [
        "access_key_list",
        "s3_user_list",
        "binding_list",
        "list_objects",
        "list_object_versions",
        "policy_read",
        "cors_read",
        "versioning_read",
        "signed_url_read",
    ] {
        let derived = derive_scope_class(&map, op).expect("id '{op}' should derive a class");
        assert_eq!(derived, "read", "id '{op}'");
        // A write-only token does not reach it...
        assert!(
            !scope_class_matches(derived, "write:adapters"),
            "write-only token wrongly admitted to '{op}'"
        );
        // ...while a read token does.
        assert!(
            scope_class_matches(derived, "read:adapters"),
            "read token wrongly refused '{op}'"
        );
    }
}

/// A caller-supplied `operation_to_scope` cannot un-know a mutating boundary
/// verb, even though it replaces the map wholesale.
///
/// Serde replaces a map wholesale, so a config that names `operation_to_scope` in
/// order to add one domain verb drops every mutating verb it did not repeat — and
/// a mutating verb the derivation cannot recognize is the one omission that
/// LOOSENS enforcement: the derivation trusts a lone recognized boundary, so an
/// id whose other boundary is a read verb would derive `read` unopposed and a
/// read-only token would reach a mutating operation.
///
/// The guarantee therefore does not come from the map at all. `derive_scope_class`
/// reads `MUTATING_BOUNDARY_VERBS` directly, which is what makes the wholesale
/// replacement safe and keeps the effective map exactly what the operator wrote.
#[test]
fn a_custom_operation_map_cannot_drop_the_mutating_boundary_verbs() {
    // Exactly the shape an operator writes to add a domain verb: read entries
    // only, no mutating verbs at all.
    let cfg: ScopeEnforcementConfig =
        serde_saphyr::from_str("operation_to_scope:\n  inspect: read\n  list: read\n")
            .expect("config should deserialize");

    // The caller's own entries are honoured.
    assert_eq!(
        cfg.operation_to_scope.get("inspect").map(String::as_str),
        Some("read")
    );
    // The map really is only what the caller wrote — no mutating verb is merged
    // back into it.
    assert_eq!(
        cfg.operation_to_scope.get("purge"),
        None,
        "nothing may be merged into a caller-supplied map"
    );
    // And yet a read boundary opposite a mutating one still yields a refusal
    // rather than an unopposed `read`, because the derivation recognizes the verb
    // without the map's help.
    assert_eq!(
        derive_scope_class(&cfg.operation_to_scope, "list_objects_purge"),
        None,
        "a mutating tail must still contradict the read head"
    );
    // The whole default set, not just `purge`.
    for verb in MUTATING_BOUNDARY_VERBS {
        assert_eq!(
            derive_scope_class(&cfg.operation_to_scope, &format!("list_objects_{verb}")),
            None,
            "mutating verb '{verb}' must still contradict a read head"
        );
    }
}

/// A stricter `default_unmapped_scope` is honoured for a bare mutating verb the
/// operator's map omits.
///
/// The mutating verbs must NOT be merged into the operator's map. Merging them
/// with the literal class `write` would make every one of them a verbatim map
/// hit, and a hit never consults `default_unmapped_scope` — a deployment with a
/// stricter fallback would silently get `write` for `delete`, `create`, `purge`
/// and their neighbours, admitting a `write:`-scoped token to operations it had
/// put behind `admin`. Merging `default_unmapped_scope` instead is the
/// mirror-image bug: a deployment whose fallback IS its read class would class
/// every mutating verb `read`, and `list_objects_purge` would AGREE on `read`
/// instead of refusing. Not merging avoids both — the fallback is applied at
/// the point that already knows it.
#[test]
fn a_stricter_default_unmapped_scope_is_honoured_for_an_omitted_mutating_verb() {
    let mut scope_cfg: ScopeEnforcementConfig =
        serde_saphyr::from_str("operation_to_scope:\n  list: read\n  get: read\n")
            .expect("config should deserialize");
    scope_cfg.default_unmapped_scope = "admin".to_owned();
    let config = Arc::new(AuthZResolverPluginConfig {
        vendor: "vz".to_owned(),
        scope_enforcement: scope_cfg,
        ..AuthZResolverPluginConfig::default()
    });
    let e = ScopeEnforcer::new(config);

    // The deployment's own fallback governs the omitted mutating verbs, so a
    // `write:`-scoped token does not reach them...
    for op in ["delete", "create", "purge", "revoke"] {
        assert_deny(e.check_scopes(&[s("write:adapters")], op));
        // ...while the class the deployment actually configured does.
        assert!(
            e.check_scopes(&[s("admin:adapters")], op).is_ok(),
            "the configured fallback class must admit '{op}'"
        );
    }

    // The read entries the operator did supply are unaffected.
    assert!(e.check_scopes(&[s("read:adapters")], "list").is_ok());
    // And a read head opposite an omitted mutating tail still lands on the
    // fallback rather than deriving `read`.
    assert_deny(e.check_scopes(&[s("read:adapters")], "list-objects-purge"));
    assert!(
        e.check_scopes(&[s("admin:adapters")], "list-objects-purge")
            .is_ok()
    );
}

/// The mirror-image case: a deployment whose `default_unmapped_scope` IS its read
/// class does not get its mutating verbs quietly reclassified as read.
///
/// Such a config is permissive by construction — it says "anything I did not map
/// is read-class" — but the derivation must still REFUSE rather than agree, so the
/// refusal is what routes the id to the fallback. Pinned because the obvious fix
/// for the test above (inject `default_unmapped_scope` into the map) would turn
/// this into an agreement on `read` and make the derivation report a derived read
/// class for a purge.
#[test]
fn a_read_class_fallback_does_not_make_a_mutating_verb_derive_read() {
    let cfg: ScopeEnforcementConfig =
        serde_saphyr::from_str("operation_to_scope:\n  list: read\n").expect("should deserialize");

    // Refusal, not `Some("read")` — the mutating tail contradicts the read head no
    // matter what the fallback happens to be named.
    assert_eq!(
        derive_scope_class(&cfg.operation_to_scope, "list_objects_purge"),
        None
    );
}
