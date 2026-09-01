#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

/// Parse with duplicate-key rejection — mirrors `toolkit`'s
/// `strict_yaml_parse`, which is what actually loads `server.yaml` in
/// production, so these tests exercise the real parser semantics.
fn parse(yaml: &str) -> Result<AuthZResolverPluginConfig, serde_saphyr::Error> {
    let opts = serde_saphyr::Options {
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        ..serde_saphyr::Options::default()
    };
    serde_saphyr::from_str_with_options(yaml, opts)
}

#[test]
// A flat list of independent default-value assertions, not nested logic.
#[allow(clippy::cognitive_complexity)]
fn minimal_yaml_applies_documented_defaults() {
    let cfg = parse(r#"vendor: "cf""#).expect("minimal config should parse");

    assert_eq!(cfg.vendor, "cf");
    assert_eq!(cfg.priority, 100);

    assert_eq!(cfg.cache.ttl_seconds, 60);
    assert_eq!(cfg.cache.max_entries.get(), 10_000);
    assert!(cfg.cache.singleflight_enabled);
    assert!(!cfg.cache.event_invalidation.enabled);

    // A PDP with no audit trail is a missing operational control, so the
    // default is ON and turning it off is the explicit choice.
    assert!(cfg.audit.enabled);

    // Fail-closed default: a resolver that cannot confirm the type it is
    // deciding about is degraded. `warn` is the explicit rollout opt-out.
    assert_eq!(cfg.gts_validation.mode, GtsValidationMode::Strict);
    assert!(cfg.gts_validation.schema_registry_endpoint.is_none());

    assert_eq!(cfg.scope_enforcement.wildcard_scope, "*");
    assert_eq!(cfg.scope_enforcement.default_unmapped_scope, "write");
    let op = &cfg.scope_enforcement.operation_to_scope;
    assert_eq!(
        op.len(),
        16,
        "default op-to-scope map should have 16 entries"
    );
    assert_eq!(op.get("read").map(String::as_str), Some("read"));
    // get/list are read-style and map to the `read` scope class.
    assert_eq!(op.get("get").map(String::as_str), Some("read"));
    assert_eq!(op.get("list").map(String::as_str), Some("read"));
    assert_eq!(op.get("write").map(String::as_str), Some("write"));
    assert_eq!(op.get("delete").map(String::as_str), Some("write"));
    assert_eq!(op.get("start").map(String::as_str), Some("write"));
    assert_eq!(op.get("stop").map(String::as_str), Some("write"));
    assert_eq!(op.get("restart").map(String::as_str), Some("write"));
    // Mutating boundary verbs. No platform operation is named any of these on its
    // own; they are in the DEFAULT map only so it spells out the whole mutating
    // vocabulary in one place. The boundary derivation does not depend on their
    // being here — it reads `MUTATING_BOUNDARY_VERBS` directly, so a custom map
    // that omits them still refuses `read_replica_create` rather than deriving
    // its `read` head unopposed.
    for verb in [
        "create", "update", "patch", "remove", "revoke", "remap", "rollback", "purge",
    ] {
        assert_eq!(
            op.get(verb).map(String::as_str),
            Some("write"),
            "mutating verb '{verb}' must be recognized"
        );
    }

    assert_eq!(cfg.capability_degradation.max_expansion_ids, 10_000);
}

#[test]
fn custom_priority_keeps_other_defaults() {
    let cfg = parse(
        r#"
vendor: "cf"
priority: 50
"#,
    )
    .expect("custom priority should parse");

    assert_eq!(cfg.priority, 50);
    assert_eq!(cfg.cache.ttl_seconds, 60);
    assert_eq!(cfg.scope_enforcement.operation_to_scope.len(), 16);
}

/// A caller-supplied map replaces the built-in one WHOLESALE — no hidden merge,
/// so the effective map is exactly what the operator wrote.
///
/// That predictability is the point: an operator who cannot compute the effective
/// map cannot compute the effective scope class either.
///
/// Replacement is safe only because the guarantee does not live in this map.
/// Dropping a *mutating* verb would otherwise loosen enforcement — the
/// derivation trusts a lone recognized boundary, so an id whose other boundary is
/// `read`/`get`/`list` would derive that read class unopposed and a read-only
/// token would reach a mutating operation. `derive_scope_class` therefore reads
/// `MUTATING_BOUNDARY_VERBS` directly instead of relying on the map, which is
/// pinned in `scope_enforcer_tests`. Merging the verbs in here would have to
/// invent a class name for each, and neither candidate is correct — see that
/// constant.
#[test]
fn custom_operation_to_scope_replaces_the_builtin_map_wholesale() {
    let cfg = parse(
        r#"
vendor: "cf"
scope_enforcement:
  operation_to_scope:
    list: read
    create: write
"#,
    )
    .expect("custom op-to-scope should parse");

    let op = &cfg.scope_enforcement.operation_to_scope;
    // The caller's own entries, verbatim.
    assert_eq!(op.get("list").map(String::as_str), Some("read"));
    assert_eq!(op.get("create").map(String::as_str), Some("write"));
    // Read verbs the caller left out are genuinely gone.
    assert_eq!(
        op.get("get"),
        None,
        "an omitted read verb must not come back"
    );
    assert_eq!(
        op.get("read"),
        None,
        "an omitted read verb must not come back"
    );
    // And so are the mutating verbs: nothing is merged back. Their absence no
    // longer loosens anything, because the derivation does not read them from
    // here.
    for verb in ["purge", "delete", "remove", "revoke", "rollback", "patch"] {
        assert_eq!(
            op.get(verb),
            None,
            "mutating verb '{verb}' must not be merged into a custom map"
        );
    }
    // Exactly the two entries supplied, nothing else.
    assert_eq!(op.len(), 2);
}

#[test]
fn unknown_root_field_rejected() {
    let err = parse(r#"vendr: "cf""#).expect_err("typo at root should fail");
    let msg = err.to_string();
    assert!(msg.contains("vendr"), "error should name `vendr`: {msg}");
}

#[test]
fn unknown_top_level_section_rejected() {
    let err = parse(
        r#"
vendor: "cf"
cach:
  ttl_seconds: 30
"#,
    )
    .expect_err("typo at section level should fail");
    let msg = err.to_string();
    assert!(msg.contains("cach"), "error should name `cach`: {msg}");
}

#[test]
fn unknown_field_inside_section_rejected() {
    let err = parse(
        r#"
vendor: "cf"
cache:
  ttl_secnds: 30
"#,
    )
    .expect_err("typo inside section should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("ttl_secnds"),
        "error should name `ttl_secnds`: {msg}"
    );
}

#[test]
fn gts_validation_mode_roundtrip() {
    for (yaml, expected) in [
        ("strict", GtsValidationMode::Strict),
        ("warn", GtsValidationMode::Warn),
        ("off", GtsValidationMode::Off),
    ] {
        let cfg = parse(&format!(
            r#"
vendor: "cf"
gts_validation:
  mode: "{yaml}"
"#
        ))
        .unwrap_or_else(|e| panic!("mode {yaml} should parse: {e}"));
        assert_eq!(cfg.gts_validation.mode, expected);
    }
}

#[test]
fn invalid_gts_validation_mode_rejected() {
    let err = parse(
        r#"
vendor: "cf"
gts_validation:
  mode: "loud"
"#,
    )
    .expect_err("invalid mode should fail");
    let msg = err.to_string();
    assert!(msg.contains("loud"), "error should mention `loud`: {msg}");
}

/// `max_entries: 0` is refused when the config is LOADED.
///
/// It used to deserialize fine and get clamped to 10 000 inside
/// `HierarchyCache::new`, so a typo produced a large live cache nobody asked
/// for and the boot succeeded. Typing the field `NonZeroUsize` moves the
/// rejection to the only place that can still fail loudly.
#[test]
fn zero_cache_max_entries_is_rejected_at_config_load() {
    let err = parse(
        r#"
vendor: "cf"
cache:
  max_entries: 0
"#,
    )
    .expect_err("max_entries: 0 MUST NOT load");
    let rendered = err.to_string();
    assert!(
        rendered.contains("zero") || rendered.contains("nonzero") || rendered.contains("NonZero"),
        "the error should name the non-zero requirement, got: {rendered}"
    );
}
