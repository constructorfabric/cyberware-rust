//! The typed configuration of SPEC §10.3: defaults, the shapes an operator
//! writes, and what fails startup.
//!
//! The resolution *rules* are pinned by the in-source tests beside
//! `domain/policy.rs`. What this file covers is the boundary a deployment
//! touches — that absent config and `config: {}` are the same thing, that the
//! documented YAML parses, and that an invalid region stops the boot instead of
//! becoming a silently closed one.
//!
//! Values are fed as JSON rather than YAML: the two shapes are identical for
//! every field here, and `humantime` / the byte-size parse accept their strings
//! through `visit_str` regardless of the input format.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use serde_json::json;
use toolkit_gts::gts_id;

use types_registry::config::{ByteSize, ConfigError, TypesRegistryConfig};
use types_registry::domain::enums::OwnershipScope;

fn parse(value: serde_json::Value) -> TypesRegistryConfig {
    serde_json::from_value(value).expect("the config must parse")
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// `ctx.config_or_default()` makes an absent section equivalent to `config: {}`.
/// Both must land on the SPEC §10.3 defaults, so the two are asserted to be the
/// same value rather than each checked against the table separately.
#[test]
fn absent_config_and_an_empty_map_are_the_same_defaults() {
    let empty = parse(json!({}));
    let absent = TypesRegistryConfig::default();

    assert_eq!(
        empty.allow_compatibility_force,
        absent.allow_compatibility_force
    );
    assert_eq!(empty.limits, absent.limits);
    assert_eq!(empty.worker, absent.worker);
    assert!(empty.registration_policy.is_empty());
    assert!(absent.registration_policy.is_empty());
}

/// The SPEC §10.3 table, value for value. A default that drifted from the
/// document would otherwise only surface as a behaviour change.
#[test]
fn the_defaults_are_the_ones_the_spec_documents() {
    let cfg = TypesRegistryConfig::default();

    assert!(!cfg.allow_compatibility_force);
    assert_eq!(cfg.limits.authored_document.bytes(), 256 * 1024);
    assert_eq!(cfg.limits.resolved_document.bytes(), 1024 * 1024);
    assert_eq!(cfg.limits.resolution_closure, 64);
    assert_eq!(cfg.limits.batch_candidates, 100);
    assert_eq!(cfg.limits.activation_write_set, 512);
    assert_eq!(cfg.limits.page_size_default, 100);
    assert_eq!(cfg.limits.page_size_max, 1000);
    assert_eq!(cfg.worker.operation_timeout, Duration::from_mins(5));
    assert_eq!(cfg.worker.max_revalidation_attempts, 8);
}

/// The existing keys are retained (SPEC §10.3), so an existing deployment's
/// configuration keeps working across this change.
#[test]
fn the_pre_existing_keys_are_retained() {
    let cfg = parse(json!({
        "entity_id_fields": ["$id"],
        "schema_id_fields": ["$schema"],
        "entities": [],
        "local_client": { "cache": { "type_schemas": { "capacity": 8, "ttl": "1s" } } },
        "allow_compatibility_force": true,
    }));
    assert_eq!(cfg.entity_id_fields, ["$id"]);
    assert_eq!(cfg.local_client.cache.type_schemas.capacity, 8);
    assert!(cfg.allow_compatibility_force);
}

/// The four `local_client.cache.*` keys stay live in P0 — the cache is kept
/// (SPEC §8.3) and its reshaping into `freshness_window` / `store_bound` belongs
/// to T30. Asserted so that reshaping is a deliberate change and not a silent one.
#[test]
fn the_cache_keys_are_still_the_pre_t30_shape() {
    let cfg = parse(json!({
        "local_client": {
            "cache": {
                "type_schemas": { "capacity": 16, "ttl": "90s" },
                "instances": { "capacity": 32, "ttl": null },
            }
        }
    }));
    assert_eq!(cfg.local_client.cache.type_schemas.capacity, 16);
    assert_eq!(
        cfg.local_client.cache.type_schemas.ttl,
        Some(Duration::from_secs(90))
    );
    assert_eq!(cfg.local_client.cache.instances.ttl, None);
}

// ---------------------------------------------------------------------------
// The documented YAML
// ---------------------------------------------------------------------------

/// The whole §10.3 block as an operator writes it, parsed and validated in one
/// go — including the four policy entries of the matrix.
#[test]
fn the_documented_configuration_block_parses_and_validates() {
    let cfg = parse(json!({
        "allow_compatibility_force": false,
        "limits": {
            "authored_document": "256KB",
            "resolved_document": "1MB",
            "resolution_closure": 64,
            "batch_candidates": 100,
            "activation_write_set": 512,
            "page_size_default": 100,
            "page_size_max": 1000,
        },
        "registration_policy": {
            (gts_id!("acme.*")): { "allowed_vendors": ["acme"], "tenant_ownable": true },
            (gts_id!("cf.core.rg.type.v1~*")): { "allowed_vendors": ["acme"], "tenant_ownable": true },
            (gts_id!("cf.core.rg.type.v1~")): { "allowed_vendors": [], "tenant_ownable": false },
            (gts_id!("cf.toolkit.plugins.plugin.v1~*")): {
                "allowed_vendors": ["*"], "tenant_ownable": false,
            },
        },
        "worker": {
            "operation_timeout": "5m",
            "max_revalidation_attempts": 8,
        },
    }));

    assert_eq!(cfg.limits.authored_document.bytes(), 256 * 1024);
    assert_eq!(cfg.registration_policy.len(), 4);
    let policy = cfg.validate().expect("the documented policy must compile");
    assert_eq!(policy.len(), 4);
}

/// An entry may name either parameter or both, and omission is preserved rather
/// than defaulted — which is what makes "a matching entry that omits a parameter
/// is skipped" expressible at all.
#[test]
fn an_entry_may_omit_either_parameter() {
    let cfg = parse(json!({
        "registration_policy": {
            (gts_id!("acme.*")): { "allowed_vendors": ["acme"] },
            (gts_id!("zeta.*")): { "tenant_ownable": true },
            (gts_id!("omega.*")): {},
        }
    }));
    let acme = &cfg.registration_policy[gts_id!("acme.*")];
    assert_eq!(
        acme.allowed_vendors.as_deref(),
        Some(["acme".to_owned()].as_slice())
    );
    assert_eq!(acme.tenant_ownable, None);

    let zeta = &cfg.registration_policy[gts_id!("zeta.*")];
    assert_eq!(zeta.allowed_vendors, None);
    assert_eq!(zeta.tenant_ownable, Some(true));

    let omega = &cfg.registration_policy[gts_id!("omega.*")];
    assert_eq!(omega.allowed_vendors, None);
    assert_eq!(omega.tenant_ownable, None);
}

/// A `tenant_ownable`-carrying configuration starts cleanly — refusing it would
/// fail the boot of a valid, P1-ready deployment — and still admits no
/// tenant-owned candidate (SPEC §10.3).
#[test]
fn a_config_carrying_tenant_ownable_starts_and_admits_no_tenant_ownership() {
    let cfg = parse(json!({
        "registration_policy": {
            (gts_id!("acme.*")): { "allowed_vendors": ["acme"], "tenant_ownable": true },
        }
    }));
    let policy = cfg
        .validate()
        .expect("a P1-ready configuration must start cleanly");

    let candidate = gts::GtsId::try_new(gts_id!("acme.crm.customer.type.v1~")).expect("identifier");
    assert!(policy.admits(&candidate, OwnershipScope::Global).is_ok());
    let refusal = policy
        .admits(&candidate, OwnershipScope::Tenant)
        .expect_err("tenant ownership is not available in P0");
    assert_eq!(refusal.parameter, "tenant_ownable");
}

// ---------------------------------------------------------------------------
// Byte sizes
// ---------------------------------------------------------------------------

/// Both spellings SPEC §10.3 uses, plus the forms an operator may reasonably
/// write. Suffixes are binary multiples; a bare integer is bytes.
#[test]
fn byte_sizes_accept_the_documented_and_the_plain_forms() {
    let cases: Vec<(serde_json::Value, usize)> = vec![
        (json!("256KB"), 256 * 1024),
        (json!("1MB"), 1024 * 1024),
        (json!("64MB"), 64 * 1024 * 1024),
        (json!("1GB"), 1024 * 1024 * 1024),
        (json!("512"), 512),
        (json!("512B"), 512),
        (json!("1 MiB"), 1024 * 1024),
        (json!("1mb"), 1024 * 1024),
        (json!(4096), 4096),
    ];
    for (input, expected) in cases {
        let got: ByteSize =
            serde_json::from_value(input.clone()).unwrap_or_else(|e| panic!("{input}: {e}"));
        assert_eq!(got.bytes(), expected, "for {input}");
    }
}

/// A malformed size is a parse failure rather than a silent zero, which would
/// refuse every document at admission.
#[test]
fn a_malformed_byte_size_fails_to_parse() {
    for bad in ["", "KB", "1.5MB", "-1"] {
        let got: Result<ByteSize, _> = serde_json::from_value(json!(bad));
        assert!(got.is_err(), "'{bad}' must not parse");
    }
}

/// Well-formed byte-count syntax with an unsupported unit is rejected rather
/// than silently interpreted as bytes or rounded to the nearest known unit.
#[test]
fn an_unsupported_byte_size_unit_fails_to_parse() {
    for bad in ["1TB", "1XB"] {
        let got: Result<ByteSize, _> = serde_json::from_value(json!(bad));
        assert!(got.is_err(), "'{bad}' must reject its unsupported unit");
    }
}

// ---------------------------------------------------------------------------
// What fails startup
// ---------------------------------------------------------------------------

/// An invalid region fails startup with the region named. A skipped entry would
/// read as a closed region and leave an operator with a refusal and no cause.
#[test]
fn an_invalid_policy_region_fails_startup_naming_the_region() {
    let cfg = parse(json!({
        "registration_policy": { "gts.*.crm.customer.type.v1~": { "allowed_vendors": ["acme"] } }
    }));
    let err = cfg
        .validate()
        .expect_err("a mid-string wildcard must fail startup");
    match err {
        ConfigError::Policy(inner) => {
            let text = inner.to_string();
            assert!(
                text.contains("gts.*.crm.customer.type.v1~"),
                "the message must name the region, got {text}"
            );
        }
        ConfigError::Limits(msg) => panic!("expected a policy error, got a limits error: {msg}"),
        ConfigError::Worker(msg) => panic!("expected a policy error, got a worker error: {msg}"),
    }
}

/// An unknown key is refused rather than ignored: `deny_unknown_fields` is what
/// turns a typo into a failed boot instead of a setting that never applied.
#[test]
fn an_unknown_key_is_refused() {
    let got: Result<TypesRegistryConfig, _> =
        serde_json::from_value(json!({ "limits": { "batch_candidate": 10 } }));
    assert!(got.is_err(), "a misspelled limit must not be ignored");

    let got: Result<TypesRegistryConfig, _> =
        serde_json::from_value(json!({ "allow_compatibility_forced": true }));
    assert!(got.is_err());
}

/// A default page size above the maximum is a configuration that cannot be
/// honoured: every unqualified request would be refused by `page_size_max`.
#[test]
fn a_default_page_size_above_the_maximum_fails_startup() {
    let cfg = parse(json!({ "limits": { "page_size_default": 500, "page_size_max": 100 } }));
    let err = cfg.validate().expect_err("an unhonourable pair must fail");
    assert!(matches!(err, ConfigError::Limits(_)), "got {err}");
}

/// Zero is refused for both page sizes: a zero default would page nothing
/// forever, and a zero maximum would refuse every request.
#[test]
fn a_zero_page_size_fails_startup() {
    for pair in [
        json!({ "page_size_default": 0 }),
        json!({ "page_size_max": 0 }),
    ] {
        let cfg = parse(json!({ "limits": pair }));
        assert!(
            cfg.validate().is_err(),
            "a zero page size must fail startup: {pair}"
        );
    }
}

// ---------------------------------------------------------------------------
// Keys P0 accepts and does not enforce
// ---------------------------------------------------------------------------

/// The shipped defaults name nothing: a deployment that configures nothing is not
/// told about limits it never asked for.
#[test]
fn the_defaults_report_no_inert_keys() {
    assert!(TypesRegistryConfig::default().inert_limit_keys().is_empty());
    assert!(parse(json!({})).inert_limit_keys().is_empty());
}

/// Every key P0 parses without acting on, one at a time, each reported under the
/// name an operator wrote. The list is exhaustive on purpose: when a task binds one
/// of these, its line here fails, which is the reminder to move the key out of the
/// list and out of the "accepted, not enforced" docstring.
#[test]
fn each_unenforced_key_is_named_when_it_is_moved_off_its_default() {
    for (limits, expected) in [
        (
            json!({ "page_size_default": 50 }),
            "limits.page_size_default",
        ),
        (json!({ "page_size_max": 500 }), "limits.page_size_max"),
    ] {
        let cfg = parse(json!({ "limits": limits.clone() }));
        assert_eq!(
            cfg.inert_limit_keys(),
            vec![expected],
            "setting {limits} must be reported as inert",
        );
    }

    let worker = json!({ "operation_timeout": "30s" });
    let cfg = parse(json!({ "worker": worker }));
    assert_eq!(
        cfg.inert_limit_keys(),
        vec!["worker.operation_timeout"],
        "setting {worker} must be reported as inert",
    );
}

/// The keys that *are* enforced are never reported, whatever they are set to —
/// otherwise the warning would train an operator to ignore it.
#[test]
fn the_enforced_limits_are_never_reported_as_inert() {
    let cfg = parse(json!({
        "limits": {
            "authored_document": "1MB", "batch_candidates": 7, "activation_write_set": 8,
            "resolved_document": "2MB", "resolution_closure": 128
        },
        "worker": { "max_revalidation_attempts": 3 }
    }));
    assert!(cfg.inert_limit_keys().is_empty());
    assert_eq!(cfg.limits.authored_document.bytes(), 1024 * 1024);
    assert_eq!(cfg.limits.batch_candidates, 7);
    assert_eq!(cfg.limits.resolved_document.bytes(), 2 * 1024 * 1024);
    assert_eq!(cfg.limits.resolution_closure, 128);
    assert_eq!(
        cfg.limits.activation_write_set, 8,
        "T14 enforces this one: the reverse-impact refusal and the CTE's depth cap"
    );
    assert_eq!(
        cfg.worker.max_revalidation_attempts, 3,
        "T15 enforces this one: the bound on the revalidation loop"
    );
}

/// A configuration that sets several of them is reported once, in full: an operator
/// fixing one key should not have to reboot to discover the next.
#[test]
fn several_inert_keys_are_reported_together() {
    let cfg = parse(json!({
        "limits": { "page_size_default": 50, "page_size_max": 500 },
        "worker": { "operation_timeout": "10m" }
    }));
    assert_eq!(
        cfg.inert_limit_keys(),
        vec![
            "limits.page_size_default",
            "limits.page_size_max",
            "worker.operation_timeout",
        ],
    );
}

#[test]
fn a_zero_enforced_limit_fails_startup() {
    for limits in [
        json!({ "batch_candidates": 0 }),
        json!({ "authored_document": 0 }),
        json!({ "authored_document": "0KB" }),
        json!({ "resolved_document": 0 }),
        json!({ "resolution_closure": 0 }),
        json!({ "activation_write_set": 0 }),
    ] {
        let cfg = parse(json!({ "limits": limits.clone() }));
        let err = cfg
            .validate()
            .expect_err(&format!("a zero limit must fail startup: {limits}"));
        assert!(matches!(err, ConfigError::Limits(_)), "got {err}");
    }
}

#[test]
fn the_minimum_positive_resolution_budgets_are_valid_configuration() {
    let cfg = parse(json!({ "limits": { "resolved_document": 1, "resolution_closure": 1 } }));
    cfg.validate().expect("positive budgets are valid");
    assert!(cfg.inert_limit_keys().is_empty());
}

#[test]
fn a_zero_revalidation_budget_fails_startup() {
    let cfg = parse(json!({ "worker": { "max_revalidation_attempts": 0 } }));
    let err = cfg
        .validate()
        .expect_err("a zero attempt budget must fail startup");
    assert!(matches!(err, ConfigError::Worker(_)), "got {err}");
}

/// The defaults themselves validate. Trivial to state and the one case a
/// deployment always exercises.
#[test]
fn the_defaults_validate() {
    TypesRegistryConfig::default()
        .validate()
        .expect("the shipped defaults must start");
}
