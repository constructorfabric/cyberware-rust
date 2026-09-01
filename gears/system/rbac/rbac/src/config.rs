//! Typed module configuration.
//!
//! RBAC runs whenever its `rbac:` entry is present in the `gears:`
//! block — like every other module. To disable it, remove the entry
//! (there is no `enabled` switch). `init()` no-ops only when the module
//! is absent from config entirely; see [`crate::module`].
//!
//! Example:
//! ```yaml
//! rbac:
//!   database:
//!     server: "pg_main"
//!   config: {}
//! ```
//!
//! `platform_admin_subject_id` is optional; when set, the principal receives
//! the built-in `Owner` role at scope `/` on first boot. It is the identity of
//! whoever administers the platform, so RBAC neither invents nor discovers it:
//! the host supplies it. Hosts typically inject it at startup from an
//! environment variable or a mounted secret rather than committing it to a
//! config file, and the same value usually drives the identity provider's
//! admin binding so the two side-effects cannot target different subjects.
//! When absent, the bootstrap step is skipped with a `WARN` log.

use serde::Deserialize;

use crate::domain::builtin_roles_catalog::TargetSpec;

/// A built-in role granted to a principal at root scope on every boot.
///
/// Two kinds of actor need a grant before anyone can hand out roles through the
/// API. In-process system actors (an `IdP` plugin writing per-realm admin
/// secrets, a collector emitting usage records) need one so their writes are
/// authorized through ordinary policy instead of a PEP bypass; human operators
/// need one so a fresh deployment comes up with somebody able to administer it.
/// Which actors exist, and under which subject ids, belongs to the deployment
/// rather than to RBAC, so both lists are configured rather than compiled in.
///
/// The list a grant appears in decides its `principal_type`:
/// [`RbacServiceConfig::service_principal_grants`] writes `ServicePrincipal`,
/// [`RbacServiceConfig::user_grants`] writes `User`. That type must match what
/// the caller's token classifies as — a grant filed under the wrong one is
/// never found, and the request is denied with no diagnostic anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleGrant {
    /// Name of the built-in role to grant, exactly as the catalog spells it
    /// (e.g. `"Credstore Secret Operator"`). Validated at `init()`: an unknown
    /// name — or one that will not be seeded because
    /// [`RbacServiceConfig::seed_integration_roles`] is off — aborts startup
    /// rather than inserting an assignment pointing at a missing role.
    pub role: String,
    /// Opaque subject id of the principal receiving the grant. Matched
    /// verbatim against the `subject_id` the PDP sees, so it must be whatever
    /// the actor actually authenticates as.
    pub principal_id: String,
}

/// GTS families the built-in roles grant, for the two rules RBAC cannot know
/// on its own.
///
/// `Owner` grants everything the platform publishes; `Contributor` and
/// `Reader` grant its resource plane. Both are named by whoever runs the
/// platform: a deployment that registers `gts.vendor.*` types and inherited the
/// compiled-in `gts.cf.*` would get built-in roles that authorize nothing at
/// all. Defaults keep the Constructor Fabric families, so a `cf` platform
/// needs no configuration.
///
/// Both settings are **lists**, because a fork normally needs two families at
/// once: its own, and `gts.cf.*` for the platform's internal types. RBAC's own
/// `role_definition` / `role_assignment` types are `gts.cf.core.…`, so a
/// `platform` list that omits them leaves `Owner` unable to administer RBAC
/// itself — `init()` logs a warning when no entry covers them.
///
/// Each entry is a family wildcard (`gts.<vendor>.*`, `gts.<vendor>.<pkg>.*`)
/// or a concrete type id; the shape is checked at `init()`, and an empty list
/// is refused. The bare `gts.*` is refused for the same reason the permission
/// matcher refuses it — "every type of every vendor" is not a deliberate
/// grant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuiltinRoleTargets {
    /// What `Owner` grants. Default `["gts.cf.*"]`.
    pub platform: Vec<String>,
    /// What `Contributor` and `Reader` grant. Default
    /// `["gts.cf.resources.*"]`.
    ///
    /// Nothing in this repository registers a type under the default family,
    /// so with it those two roles authorize nothing — point this at your own
    /// resource plane to give them meaning.
    pub resources_family: Vec<String>,
}

impl Default for BuiltinRoleTargets {
    fn default() -> Self {
        Self {
            platform: vec![
                crate::domain::builtin_roles_catalog::DEFAULT_PLATFORM_TARGET.to_owned(),
            ],
            resources_family: vec![
                crate::domain::builtin_roles_catalog::DEFAULT_RESOURCES_FAMILY_TARGET.to_owned(),
            ],
        }
    }
}

impl BuiltinRoleTargets {
    /// Resolve a catalog slot into the targets to seed. A [`TargetSpec::Fixed`]
    /// rule yields exactly one target; a slot yields one per configured entry,
    /// so a rule over a slot expands into as many permission rules.
    #[must_use]
    pub fn resolve(&self, spec: TargetSpec) -> Vec<&str> {
        match spec {
            TargetSpec::Fixed(target) => vec![target],
            TargetSpec::Platform => self.platform.iter().map(String::as_str).collect(),
            TargetSpec::ResourcesFamily => {
                self.resources_family.iter().map(String::as_str).collect()
            }
        }
    }
}

/// Gear-level configuration loaded by `figment`. Derived `Default` keeps every
/// deployment-specific field empty: no platform admin, no service-principal
/// grants, no integration roles.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RbacServiceConfig {
    /// Optional opaque subject ID for the platform administrator. When set,
    /// `init()` creates an Owner-at-`/` `role_assignments` row idempotently.
    ///
    /// MUST default to `None` — a phantom default would hand someone the
    /// platform. Populate it from the host: inject the value at startup from
    /// an environment variable or a mounted secret. Prefer that over a literal
    /// in a config file, so the admin identity is not committed to a repo.
    pub platform_admin_subject_id: Option<String>,
    /// Seed the built-in roles whose permission targets belong to *other*
    /// gears — `Credstore Secret Operator` (credstore secrets) and
    /// `Usage Emitter` (usage-collector records). Default `false`: a
    /// deployment without those gears would otherwise inherit roles that
    /// authorize resource types nobody registered, and RBAC should not decide
    /// which neighbours a platform runs.
    ///
    /// Turning this off never removes rows: the seeder upserts and never
    /// deletes, so an existing deployment keeps roles seeded by an earlier
    /// boot. It only stops *fresh* installs from getting them.
    pub seed_integration_roles: bool,
    /// Service-principal grants written idempotently at every boot. Empty by
    /// default — see [`RoleGrant`]. Each entry is
    /// `{ role, principal_id }`; the row is `created_by = "system-bootstrap"`
    /// at scope `/`.
    pub service_principal_grants: Vec<RoleGrant>,
    /// Grants written idempotently at every boot for principals that
    /// authenticate as human users: `principal_type = "User"`, scope `/`.
    ///
    /// Empty by default. Use it when a fresh deployment must come up with
    /// somebody already able to sign in and administer it beyond the single
    /// `Owner` that [`Self::platform_admin_subject_id`] grants — a read-only
    /// operator, say, or a second administrator.
    ///
    /// Scope is always `/`, as it is for service principals. A tenant-scoped
    /// grant would name a tenant that need not exist yet when RBAC starts, and
    /// nothing here can check that: the bootstrap writes straight to the table,
    /// bypassing the scope-existence validation the REST path performs. Those
    /// grants belong on `POST /rbac/v1/role-assignments`.
    pub user_grants: Vec<RoleGrant>,
    /// GTS families the built-in roles grant — see [`BuiltinRoleTargets`].
    pub builtin_role_targets: BuiltinRoleTargets,
    /// Display-name resolution for role-assignment reads. Safe to leave
    /// at its defaults; see [`PrincipalNamesConfig`].
    pub principal_names: PrincipalNamesConfig,
}

/// Configuration rejected at `init()`. Named for the gear so it cannot be
/// confused with `toolkit`'s config-loading error at the call site.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RbacConfigError {
    /// A grant left `role` or `principal_id` blank. `list` names which of the
    /// two grant lists it came from.
    #[error("config: {list}[{index}].{field} must be non-empty")]
    BlankGrantField {
        list: &'static str,
        index: usize,
        field: &'static str,
    },
    /// A grant names a role this deployment will not seed.
    #[error(
        "config: {list}[{index}].role {role:?} is not a built-in role this \
         deployment seeds (known: {known:?}); set seed_integration_roles if it is an \
         integration role"
    )]
    UnknownGrantRole {
        list: &'static str,
        index: usize,
        role: String,
        known: Vec<&'static str>,
    },
    /// A built-in role target is neither a GTS type id nor a family wildcard.
    #[error(
        "config: builtin_role_targets.{field}[{index}] {value:?} must be a GTS type id ending \
         in `~` or a family wildcard ending in `*`, with at least one segment before the `*`"
    )]
    InvalidRoleTarget {
        field: &'static str,
        index: usize,
        value: String,
    },
    /// A built-in role target list was left empty.
    #[error(
        "config: builtin_role_targets.{field} must name at least one GTS family or type; \
         an empty list seeds a built-in role that authorizes nothing"
    )]
    EmptyRoleTargets { field: &'static str },
    /// A `principal_names` bound would invert the feature's cost model. The
    /// message comes from [`PrincipalNamesConfig::validate`] and names the
    /// offending field.
    #[error("config: {0}")]
    InvalidPrincipalNames(String),
}

impl RbacServiceConfig {
    /// Validate the configuration before anything is written.
    ///
    /// # Errors
    /// [`RbacConfigError`] when a service-principal grant is blank or names a role
    /// that will not exist. Both would otherwise surface as a dangling
    /// `role_assignments` row or a silent no-op, and a grant is a privilege —
    /// it must never be created by accident.
    pub fn validate(&self) -> Result<(), RbacConfigError> {
        validate_role_targets("platform", &self.builtin_role_targets.platform)?;
        validate_role_targets(
            "resources_family",
            &self.builtin_role_targets.resources_family,
        )?;

        self.validate_grants("service_principal_grants", &self.service_principal_grants)?;
        self.validate_grants("user_grants", &self.user_grants)?;

        self.principal_names
            .validate()
            .map_err(RbacConfigError::InvalidPrincipalNames)?;
        Ok(())
    }

    /// Shared validation for both grant lists. `list` is the config key, so
    /// the error names the list the operator actually wrote.
    fn validate_grants(
        &self,
        list: &'static str,
        grants: &[RoleGrant],
    ) -> Result<(), RbacConfigError> {
        for (index, grant) in grants.iter().enumerate() {
            if grant.role.trim().is_empty() {
                return Err(RbacConfigError::BlankGrantField {
                    list,
                    index,
                    field: "role",
                });
            }
            if grant.principal_id.trim().is_empty() {
                return Err(RbacConfigError::BlankGrantField {
                    list,
                    index,
                    field: "principal_id",
                });
            }
            // The DOMAIN catalog, deliberately not `infra::seeder`: that module
            // already imports `crate::config` for `BuiltinRoleTargets`, so
            // validating grants through it made config depend on storage and
            // storage depend on config. A grant name is a domain fact.
            if crate::domain::builtin_roles_catalog::role_id_by_name(
                &grant.role,
                self.seed_integration_roles,
            )
            .is_none()
            {
                return Err(RbacConfigError::UnknownGrantRole {
                    list,
                    index,
                    role: grant.role.clone(),
                    known: crate::domain::builtin_roles_catalog::role_names(
                        self.seed_integration_roles,
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Validate one `builtin_role_targets` list: non-empty, every entry a
/// well-formed target. An empty list is refused because it would seed a
/// built-in role with no permission rules at all — a role that exists,
/// assigns cleanly, and authorizes nothing.
fn validate_role_targets(field: &'static str, values: &[String]) -> Result<(), RbacConfigError> {
    if values.is_empty() {
        return Err(RbacConfigError::EmptyRoleTargets { field });
    }
    for (index, value) in values.iter().enumerate() {
        validate_role_target(field, index, value)?;
    }
    Ok(())
}

/// Accept a concrete GTS type id (`gts.….v1~`) or a family wildcard
/// (`gts.….*`), delegating the grammar to the platform parser rather than
/// re-deriving it from string suffixes. A bare `gts.*` is refused: the
/// permission matcher applies the same one-segment floor, so accepting it here
/// would seed a rule the matcher then treats as unreachable.
fn validate_role_target(
    field: &'static str,
    index: usize,
    value: &str,
) -> Result<(), RbacConfigError> {
    let invalid = || RbacConfigError::InvalidRoleTarget {
        field,
        index,
        value: value.to_owned(),
    };

    let Some(body) = value.strip_prefix(toolkit_gts::GTS_ID_PREFIX) else {
        return Err(invalid());
    };
    let parsed = gts::GtsOps::parse_id(value);
    if !parsed.ok {
        return Err(invalid());
    }
    if parsed.is_wildcard {
        if body.trim_end_matches('*').is_empty() {
            return Err(invalid());
        }
        return Ok(());
    }
    if parsed.is_type == Some(true) {
        return Ok(());
    }
    Err(invalid())
}

/// Display-name resolution for role-assignment reads.
///
/// Every knob here bounds *upstream cost*, because naming a user is not
/// a point read: account management serves a user listing out of the
/// tenant's Keycloak group membership and re-drains that membership on
/// every call. So the reader pages once over the membership and caches
/// what it saw, and `max_pages_per_tenant` is what keeps a very large
/// tenant from turning one page render into a long series of drains. Ids
/// a truncated pass did not cover fall back to point lookups, themselves
/// bounded by `max_point_lookups_per_tenant`.
///
/// Those two bound one *tenant*. A page can carry principals from many
/// tenants, so the per-tenant bounds only compose into a per-request
/// bound through `max_lookup_tenants_per_request` (which caps the
/// multiplier) and `resolve_timeout_ms` (which caps wall clock whatever
/// the counts say). Without both, a root-scope caller listing
/// assignments spread over dozens of tenants would not degrade — it
/// would simply take as long as the sum of every tenant's budget.
///
/// Names are display-only, so every bound here degrades to "no name",
/// never to an error or a short page.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrincipalNamesConfig {
    /// Master switch. `false` serves every row without names and
    /// resolves no upstream client at all.
    pub enabled: bool,
    /// Positive/negative cache TTL, in seconds. A rename becomes visible
    /// within one TTL; 30s matches the TTL the gear's other cached
    /// upstream reader already uses.
    pub cache_ttl_seconds: u64,
    /// Cache size bound. Reaching it clears the cache wholesale rather
    /// than evicting per entry — simpler than an LRU, and the TTL makes
    /// a cold cache self-healing.
    pub cache_max_entries: usize,
    /// Page budget for one membership pass, in account-management pages
    /// (200 users each). The default names the first 1000 members of a
    /// tenant from a single pass.
    pub max_pages_per_tenant: u32,
    /// Cap on the per-id fallback lookups issued after a
    /// *budget-truncated* pass. Each one costs another membership drain
    /// upstream, so a page full of principals in a very large tenant must
    /// not be allowed to issue one per row.
    pub max_point_lookups_per_tenant: u32,
    /// Cap on the number of **distinct lookup tenants** one request may
    /// resolve names in. This is the knob that turns the two per-tenant
    /// budgets above into a per-request bound: without it a page whose
    /// principals live in N tenants costs N × (pages + point lookups)
    /// upstream calls, and N is chosen by whoever writes the assignments,
    /// not by the operator.
    ///
    /// With the defaults the worst case per request is
    /// `8 × (5 + 25) = 240` upstream calls; the resolve timeout bounds
    /// what that can actually cost in wall clock. Tenants beyond the cap
    /// are simply not looked up — their rows keep their ids, which is the
    /// documented degradation and not an error.
    pub max_lookup_tenants_per_request: u32,
    /// Wall-clock bound on display-name resolution for **one request**,
    /// in milliseconds, covering every lookup the hydrator makes (root
    /// tenant, user passes, group listing, local role read).
    ///
    /// This is a hang-stopper, not a latency target: on expiry the page
    /// is served with whatever names resolved before the deadline, so a
    /// generous value costs nothing on a healthy cluster and a stingy one
    /// blanks names on a cold cache. A single membership drain of a large
    /// tenant is already a second or so, and a cold page can need
    /// several, hence seconds rather than milliseconds.
    pub resolve_timeout_ms: u64,
}

impl Default for PrincipalNamesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_ttl_seconds: 30,
            cache_max_entries: 10_000,
            max_pages_per_tenant: 5,
            max_point_lookups_per_tenant: 25,
            max_lookup_tenants_per_request: 8,
            resolve_timeout_ms: 5_000,
        }
    }
}

impl PrincipalNamesConfig {
    /// Cache TTL as a [`std::time::Duration`].
    #[must_use]
    pub fn cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.cache_ttl_seconds)
    }

    /// Per-request resolution deadline as a [`std::time::Duration`].
    #[must_use]
    pub fn resolve_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.resolve_timeout_ms)
    }

    /// Reject operator overrides that would silently invert the
    /// feature's cost model.
    ///
    /// Every knob here defaults to a sane value, so this only ever fires
    /// on an explicit override — which is exactly the case the defaults
    /// cannot protect. The failure modes are not "slower than intended"
    /// but the opposite of what the knob is named for:
    ///
    /// * `max_pages_per_tenant = 0` runs no membership pass at all, so a
    ///   read degenerates into up to `max_point_lookups_per_tenant`
    ///   single-id lookups per tenant — one full Keycloak drain each.
    ///   That is precisely the N+1 the pass exists to prevent.
    /// * `cache_max_entries = 0` makes the cache clear itself on every
    ///   insert, so nothing is ever served from memory and every render
    ///   pays full price.
    /// * a zero TTL, a zero tenant cap or a zero timeout each disable
    ///   naming entirely while looking like a tuning value.
    ///
    /// # Errors
    ///
    /// A human-readable message naming the offending field. Returned as
    /// `String` to match the `validate()` convention the other gears'
    /// configs use.
    pub fn validate(&self) -> Result<(), String> {
        // A disabled feature resolves nothing, so its bounds are inert
        // and an operator must not be blocked from parking values there.
        if !self.enabled {
            return Ok(());
        }
        if self.cache_ttl_seconds == 0 {
            return Err(
                "principal_names.cache_ttl_seconds must be > 0 (a zero TTL expires \
                        every entry immediately, so every read pays a full membership drain)"
                    .to_owned(),
            );
        }
        if self.cache_max_entries == 0 {
            return Err(
                "principal_names.cache_max_entries must be > 0 (a zero bound clears \
                        the cache on every insert, which disables caching entirely)"
                    .to_owned(),
            );
        }
        if self.max_pages_per_tenant == 0 {
            return Err(
                "principal_names.max_pages_per_tenant must be > 0 (a zero page budget \
                        runs no membership pass, degrading every read into one full drain \
                        per principal - the N+1 this feature exists to avoid)"
                    .to_owned(),
            );
        }
        if self.max_point_lookups_per_tenant == 0 {
            return Err(
                "principal_names.max_point_lookups_per_tenant must be > 0 (a \
                        budget-truncated pass would then be unable to name anything)"
                    .to_owned(),
            );
        }
        if self.max_lookup_tenants_per_request == 0 {
            return Err(
                "principal_names.max_lookup_tenants_per_request must be > 0 (a zero \
                        cap resolves no user names at all); set principal_names.enabled = \
                        false to switch the feature off"
                    .to_owned(),
            );
        }
        if self.resolve_timeout_ms == 0 {
            return Err(
                "principal_names.resolve_timeout_ms must be > 0 (a zero deadline \
                        expires before the first lookup, so no name ever resolves); set \
                        principal_names.enabled = false to switch the feature off"
                    .to_owned(),
            );
        }
        Ok(())
    }

    // The three accessors below are the fail-safe behind `validate()`.
    // Validation is the loud path — it refuses the config at startup and
    // names the field — but a value that slips past it (a caller that
    // never validates, a future config source) must not be able to make
    // the read path *more* expensive than the defaults. Saturating to the
    // smallest working value keeps the cost model intact while the
    // operator's mistake is still visible in the logs.

    /// Cache capacity, never zero (a zero bound would clear the cache on
    /// every insert).
    #[must_use]
    pub fn cache_capacity(&self) -> usize {
        self.cache_max_entries.max(1)
    }

    /// Membership-pass page budget, never zero (a zero budget would skip
    /// the pass and fall back to one full drain per principal).
    #[must_use]
    pub fn pages_per_tenant(&self) -> u32 {
        self.max_pages_per_tenant.max(1)
    }

    /// Distinct-tenant budget for one request, never zero.
    #[must_use]
    pub fn lookup_tenants_per_request(&self) -> usize {
        self.max_lookup_tenants_per_request.max(1) as usize
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
