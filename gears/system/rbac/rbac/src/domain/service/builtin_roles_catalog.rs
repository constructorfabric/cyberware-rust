//! Canonical built-in role catalog (pure-domain data).
//!
//! Owns the answer to **what** the platform's built-in roles are. The
//! seeder under `crate::infra::seeder::BuiltinRoleSeeder` reads this
//! catalog and applies it via `SeaORM`. Every role carries a fixed
//! `UUIDv7`-shaped synthetic UUID, is inserted with `is_built_in = true`,
//! `owner_tenant_id = NULL`, `created_by = "system"`, has empty
//! `not_permissions`, and `assignable_scopes = ["/"]`.
//!
//! ## Lock-ordering invariant
//!
//! [`CANONICAL_BUILTIN_ROLES`] MUST stay sorted by `id` ascending.
//! Concurrent seeders walk this slice in order and acquire
//! `role_definitions` row locks in the same sequence — closing the
//! deadlock class `(A→B)` vs `(B→A)`.

use rbac_sdk::models::Action;
use toolkit_gts::gts_id;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::resource_types;

/// Stable system identifier stamped on `created_by` for every built-in row.
pub const SYSTEM_CREATED_BY: &str = "system";

/// Which GTS family a built-in rule grants.
///
/// Two of the built-ins are shaped by the deployment rather than by RBAC:
/// `Owner` grants everything the platform publishes, and `Contributor` /
/// `Reader` grant its resource plane. Those families are named by whoever runs
/// the platform — a fork that registers `gts.vendor.*` types would get roles that
/// authorize nothing if the wildcards were compiled in — so the catalog stores
/// a slot and `RbacServiceConfig::builtin_role_targets` fills it at seed time.
///
/// Everything else is [`TargetSpec::Fixed`]: RBAC's own types, and the two
/// integration roles' targets, which are the neighbouring gear's contract and
/// not a deployment choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSpec {
    /// A GTS target this gear (or a neighbour it names) owns outright.
    Fixed(&'static str),
    /// The platform-wide wildcard — `builtin_role_targets.platform`.
    Platform,
    /// The resource-plane family — `builtin_role_targets.resources_family`.
    ResourcesFamily,
}

/// Default for `builtin_role_targets.platform`: every Constructor Fabric type.
pub const DEFAULT_PLATFORM_TARGET: &str = gts_id!("cf.*");

/// Default for `builtin_role_targets.resources_family`: the Constructor Fabric
/// resource plane (compute, network, storage, …).
///
/// **Nothing in this repository registers a type under it**, so with the
/// default value `Contributor` and `Reader` authorize nothing. That is not a
/// defect in the roles — the resource plane lives outside this tree. A
/// deployment with its own resource types points the setting at them.
pub const DEFAULT_RESOURCES_FAMILY_TARGET: &str = gts_id!("cf.resources.*");

/// Credstore Secret Operator grant — the single credential-store secret
/// resource type. Hardcoded (RBAC does not depend on the credstore gear);
/// MUST stay in sync with `credstore_sdk::SECRET_RESOURCE_TYPE`. This is the
/// narrow least-privilege target for the in-process system service principal
/// that writes per-realm admin secrets (the `vp-idp-plugin` system actor),
/// granted instead of a PEP bypass.
pub const CREDSTORE_SECRET_TARGET: &str = gts_id!("cf.core.credstore.secret.v1~");

/// Usage Emitter grant — the Usage Collector's usage-record ingestion type.
///
/// Re-exported from the usage-collector SDK rather than restated, so the grant
/// and the resource type the collector's PEP actually enforces cannot drift.
/// The dependency is on the SDK — declarations only, no gear implementation.
/// A drifted target string would be silent: the role would seed cleanly and
/// then grant nothing, with no error at any layer.
///
/// Concrete rather than a family wildcard: the seeder writes straight to
/// `role_definitions` and never passes through the types-registry target-type
/// validator, which guards only the REST role-definition write path.
pub const USAGE_RECORD_TARGET: &str = usage_collector_sdk::USAGE_RECORD_RESOURCE;

/// Usage Emitter grant — the Usage Collector's usage-type catalog, which is
/// platform-global (one `gts_id` shared by every tenant reporting against it).
/// Re-exported from the SDK for the same reason as [`USAGE_RECORD_TARGET`].
pub const USAGE_TYPE_TARGET: &str = usage_collector_sdk::USAGE_TYPE_RESOURCE;

/// Static description of a single canonical built-in role.
#[domain_model]
pub struct CanonicalBuiltinRole {
    pub(crate) id: Uuid,
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    /// Permission rules granted by the role. Order is preserved when
    /// serialised into the JSONB column.
    pub(crate) permission_rules: &'static [PermissionRuleStatic],
    /// Scope strings written to the `assignable_scopes` JSONB column.
    pub(crate) assignable_scopes: &'static [&'static str],
    /// `true` when the role only means something alongside another gear —
    /// its permission targets are that gear's resource types, not RBAC's.
    /// Seeded only when `RbacServiceConfig::seed_integration_roles` is on, so
    /// a deployment without those gears does not inherit roles that authorize
    /// types nobody registered. See the field docs on the config.
    pub(crate) integration: bool,
}

/// Static `permission_rule_v1` shape suitable for `const` declaration.
#[domain_model]
pub struct PermissionRuleStatic {
    pub(crate) operation: &'static str,
    /// Either a target this gear owns, or a slot the deployment fills — see
    /// [`TargetSpec`].
    pub(crate) target: TargetSpec,
}

/// The canonical built-in roles, in ascending `id` order. Deliberately not
/// stated as a count: the roster grows, and a number here would rot silently
/// (it already read "four" while the slice held five).
///
/// **Lock-ordering invariant.** The slice MUST stay sorted by `id`
/// ascending — see the module-level docs.
pub const CANONICAL_BUILTIN_ROLES: &[CanonicalBuiltinRole] = &[
    CanonicalBuiltinRole {
        // 0195f2b6-0001-7000-8000-000000000001 — Owner.
        id: Uuid::from_u128(0x0195_f2b6_0001_7000_8000_0000_0000_0001_u128),
        name: "Owner",
        description: "Grants full access to platform resources",
        permission_rules: &[PermissionRuleStatic {
            operation: Action::Wildcard.as_str(),
            target: TargetSpec::Platform,
        }],
        assignable_scopes: &["/"],
        integration: false,
    },
    CanonicalBuiltinRole {
        // 0195f2b6-0002-7000-8000-000000000002 — Contributor.
        id: Uuid::from_u128(0x0195_f2b6_0002_7000_8000_0000_0000_0002_u128),
        name: "Contributor",
        description: "Grants broad management access to platform resources, but not RBAC administration",
        permission_rules: &[PermissionRuleStatic {
            operation: Action::Wildcard.as_str(),
            target: TargetSpec::ResourcesFamily,
        }],
        assignable_scopes: &["/"],
        integration: false,
    },
    CanonicalBuiltinRole {
        // 0195f2b6-0003-7000-8000-000000000003 — Reader.
        id: Uuid::from_u128(0x0195_f2b6_0003_7000_8000_0000_0000_0003_u128),
        name: "Reader",
        description: "View resources without mutating them",
        permission_rules: &[PermissionRuleStatic {
            operation: Action::Read.as_str(),
            target: TargetSpec::ResourcesFamily,
        }],
        assignable_scopes: &["/"],
        integration: false,
    },
    CanonicalBuiltinRole {
        // 0195f2b6-0004-7000-8000-000000000004 — User Access Administrator.
        id: Uuid::from_u128(0x0195_f2b6_0004_7000_8000_0000_0000_0004_u128),
        name: "User Access Administrator",
        description: "Manage role assignments and inspect role definitions",
        permission_rules: &[
            PermissionRuleStatic {
                operation: Action::Wildcard.as_str(),
                target: TargetSpec::Fixed(resource_types::ROLE_ASSIGNMENT),
            },
            PermissionRuleStatic {
                operation: Action::Read.as_str(),
                target: TargetSpec::Fixed(resource_types::ROLE_DEFINITION),
            },
        ],
        assignable_scopes: &["/"],
        integration: false,
    },
    CanonicalBuiltinRole {
        // 0195f2b6-0005-7000-8000-000000000005 — Credstore Secret Operator.
        // Narrow system grant: the in-process service principal that writes
        // per-realm admin secrets (vp-idp-plugin) is authorized here through
        // ordinary RBAC rather than a PEP bypass. read/write/delete only on
        // the credstore secret resource type — no other resource family.
        id: Uuid::from_u128(0x0195_f2b6_0005_7000_8000_0000_0000_0005_u128),
        name: "Credstore Secret Operator",
        description: "Read, write, and delete credential-store secrets (system service-principal grant)",
        permission_rules: &[
            PermissionRuleStatic {
                operation: Action::Read.as_str(),
                target: TargetSpec::Fixed(CREDSTORE_SECRET_TARGET),
            },
            PermissionRuleStatic {
                operation: Action::Write.as_str(),
                target: TargetSpec::Fixed(CREDSTORE_SECRET_TARGET),
            },
            PermissionRuleStatic {
                operation: Action::Delete.as_str(),
                target: TargetSpec::Fixed(CREDSTORE_SECRET_TARGET),
            },
        ],
        assignable_scopes: &["/"],
        integration: true,
    },
    CanonicalBuiltinRole {
        // 0195f2b6-0006-7000-8000-000000000006 — Usage Emitter.
        // Narrow grant for the metering identity an infrastructure adapter
        // runs its usage worker as (an out-of-process Service Principal, so
        // ordinary RBAC is the only way to authorize it — there is no
        // in-process trusted-actor path available to it).
        //
        // `create` is spelled as a literal, NOT `Action::Write`: the
        // permission matcher compares operations by exact equality, and the
        // PDP canonicalizes only `get`/`list` to `read`, so `create` reaches
        // the matcher verbatim. A `write` rule here would silently deny every
        // record.
        //
        // The `read` rule on the usage-type catalog is not consulted by the
        // ingest path — the collector validates a record's usage type on its
        // own authority via the plugin SPI, bypassing its own authorized
        // read wrapper. It is granted so the adapter can pre-validate a
        // `gts_id` client-side and fail fast instead of absorbing a
        // per-record not-found. One `read` covers both the get and list
        // surfaces, since the PDP canonicalizes both.
        id: Uuid::from_u128(0x0195_f2b6_0006_7000_8000_0000_0000_0006_u128),
        name: "Usage Emitter",
        description: "Create usage records and read the usage-type catalog (metering service-principal grant)",
        permission_rules: &[
            PermissionRuleStatic {
                operation: "create",
                target: TargetSpec::Fixed(USAGE_RECORD_TARGET),
            },
            PermissionRuleStatic {
                operation: Action::Read.as_str(),
                target: TargetSpec::Fixed(USAGE_TYPE_TARGET),
            },
        ],
        assignable_scopes: &["/"],
        integration: true,
    },
];

/// Compile-time enforcement of the lock-ordering invariant documented at
/// module level. A future contributor reordering the slice for
/// readability — and not noticing the docs — will trip this assertion at
/// the next `cargo build`, not silently at concurrent-seeder runtime.
/// A `debug_assert!` would not do: release builds strip it, leaving the
/// invariant enforced only in `cfg(debug_assertions)` builds.
const _ASSERT_BUILTIN_ROLES_SORTED_BY_ID: () = {
    let roles = CANONICAL_BUILTIN_ROLES;
    let mut i = 1;
    while i < roles.len() {
        assert!(
            roles[i - 1].id.as_u128() < roles[i].id.as_u128(),
            "CANONICAL_BUILTIN_ROLES MUST stay sorted by id ascending \u{2014} \
             reordering breaks the lock-ordering invariant the seeder relies on \
             (defense-in-depth today, load-bearing the moment the seed loop is \
             wrapped in an explicit transaction)."
        );
        i += 1;
    }
};

/// The roles a deployment seeds: the core roster always, the integration roles
/// only when it asked for them. Order is the catalog's (ascending `id`), which
/// the seed loop's lock ordering relies on.
///
/// These three queries are pure reads over [`CANONICAL_BUILTIN_ROLES`] and live
/// here rather than on the storage seeder: `RbacServiceConfig::validate` needs
/// them to check `service_principal_grants` / `user_grants` against real role
/// names, and reaching into `infra::seeder` for that put config validation
/// downstream of storage — while `infra::seeder` already imports `crate::config`
/// for `BuiltinRoleTargets`. A catalog change would then have been able to break
/// boot through the wrong layer.
pub fn roster(include_integration: bool) -> impl Iterator<Item = &'static CanonicalBuiltinRole> {
    CANONICAL_BUILTIN_ROLES
        .iter()
        .filter(move |role| include_integration || !role.integration)
}

/// Number of built-in roles for the given integration-role setting.
#[must_use]
pub fn role_count(include_integration: bool) -> usize {
    roster(include_integration).count()
}

/// Names of the built-in roles, in catalog order (ascending `id`).
#[must_use]
pub fn role_names(include_integration: bool) -> Vec<&'static str> {
    roster(include_integration).map(|role| role.name).collect()
}

/// `id` of the built-in role named `name`, when the deployment would seed it.
///
/// `None` for an unknown name, and for an integration role while
/// `include_integration` is off — a grant against a role that will not exist
/// must fail loudly rather than insert a dangling assignment.
#[must_use]
pub fn role_id_by_name(name: &str, include_integration: bool) -> Option<uuid::Uuid> {
    roster(include_integration)
        .find(|role| role.name == name)
        .map(|role| role.id)
}

#[cfg(test)]
#[path = "builtin_roles_catalog_tests.rs"]
mod builtin_roles_catalog_tests;
