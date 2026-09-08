//! Runtime configuration for the `AuthZ` resolver plugin.
//!
//! Every section is annotated `#[serde(default, deny_unknown_fields)]` so an
//! operator-provided YAML need only specify `vendor`, and typos (`cach:`
//! instead of `cache:`) fail at startup naming the offending field rather than
//! silently taking defaults.
//!
//! The operator-facing reference is `README.md`; `docs/DESIGN.md` section 3.11
//! records why each default is what it is.

use std::collections::HashMap;
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

/// One trusted in-process system actor: the pair a request must present to be
/// short-circuited to Allow.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedSystemActor {
    /// Exact `subject.subject_type` tag the actor presents (e.g. a
    /// `"<service>.system"` marker). Matched by equality, never by substring.
    pub subject_type: String,
    /// The actor's `subject.id`. Unforgeable by construction — it is minted
    /// in-process, never issued to a token holder — which is what makes
    /// trusting the pair safe.
    pub subject_id: uuid::Uuid,
}

/// Top-level plugin config. Read from `gears.authz-resolver-plugin.config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthZResolverPluginConfig {
    /// In-process system actors the PDP trusts: each entry is a
    /// `(subject_type, subject_id)` pair that short-circuits to Allow, skips
    /// scope enforcement, and bypasses subject-type classification.
    ///
    /// **Empty by default.** These are privilege bypasses, so nothing is
    /// trusted unless a deployment names it. Both halves must match: the
    /// subject id is the load-bearing half (an unforgeable in-process
    /// sentinel), while the type tag alone can be forged in a token. A
    /// cross-pair combination is not trusted.
    ///
    /// Configure this only for actors constructed in-process by the platform
    /// itself — for example an account-management cascade worker whose reads
    /// are PEP-gated but which holds no RBAC roles.
    pub trusted_system_actors: Vec<TrustedSystemActor>,
    /// Vendor identifier (typically `"cf"`).
    pub vendor: String,
    /// Gateway-side ranking priority (lower = higher priority). Default `100`.
    /// Typed as `i16` to match the GTS `PluginV1` field exactly, so an
    /// out-of-range value is rejected at deserialization rather than silently
    /// clamped.
    pub priority: i16,
    /// Hierarchy cache configuration — see `hierarchy_cache`.
    pub cache: CacheConfig,
    /// Audit event emission configuration — see `audit_emitter`.
    pub audit: AuditConfig,
    /// GTS type validation mode — see `gts_type_validator`.
    pub gts_validation: GtsValidationConfig,
    /// Token scope intersection rules — see `scope_enforcer`.
    pub scope_enforcement: ScopeEnforcementConfig,
    /// Large-expansion fallback bounds — see `constraint_generator`.
    pub capability_degradation: CapabilityDegradationConfig,
}

impl Default for AuthZResolverPluginConfig {
    fn default() -> Self {
        Self {
            trusted_system_actors: Vec::new(),
            vendor: String::new(),
            priority: 100,
            cache: CacheConfig::default(),
            audit: AuditConfig::default(),
            gts_validation: GtsValidationConfig::default(),
            scope_enforcement: ScopeEnforcementConfig::default(),
            capability_degradation: CapabilityDegradationConfig::default(),
        }
    }
}

/// Hierarchy/decision cache configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub ttl_seconds: u64,
    /// `NonZeroUsize` so `max_entries: 0` is refused by config loading itself.
    /// It used to be a `usize` that the cache clamped to 10 000 at construction
    /// — which turned a typo into a large live cache the operator never asked
    /// for, and never failed the boot.
    pub max_entries: NonZeroUsize,
    pub singleflight_enabled: bool,
    pub event_invalidation: EventInvalidationConfig,
}

/// Default cache capacity. `NonZeroUsize` has no literal syntax, so it is built
/// in a `const` context: the `None` arm is unreachable for a non-zero literal
/// and, being const-evaluated, could only ever fail the build — never panic at
/// runtime.
const DEFAULT_CACHE_MAX_ENTRIES: NonZeroUsize = match NonZeroUsize::new(10_000) {
    Some(n) => n,
    None => NonZeroUsize::MIN,
};

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl_seconds: 60,
            max_entries: DEFAULT_CACHE_MAX_ENTRIES,
            singleflight_enabled: true,
            event_invalidation: EventInvalidationConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventInvalidationConfig {
    pub enabled: bool,
}

/// Authorization audit emission. Enabled by default: a PDP with no audit
/// trail is a missing operational control, not a quiet default. The record goes
/// to the dedicated `cf-authz.audit` tracing target, so a deployment routes or
/// samples its volume at the subscriber rather than by turning the control off.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    pub enabled: bool,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GtsValidationMode {
    /// Default. An unknown subject or resource type is a business deny. A PDP
    /// that cannot confirm the type it is deciding about is degraded, so the
    /// safe posture is the default one and `warn` is the explicit opt-out.
    #[default]
    Strict,
    /// Unknown types are logged and allowed through — for a rollout whose type
    /// registration is still incomplete. A registry OUTAGE still errors; see
    /// `domain::gts_type_validator`.
    Warn,
    /// The registry is not consulted at all.
    Off,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GtsValidationConfig {
    pub mode: GtsValidationMode,
    pub schema_registry_endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
// Field names are the YAML/serde wire contract; the shared `_scope` suffix is
// part of the public config schema and must not be renamed to satisfy the lint.
#[allow(clippy::struct_field_names)]
pub struct ScopeEnforcementConfig {
    pub wildcard_scope: String,
    pub default_unmapped_scope: String,
    /// Replaced wholesale by a caller-supplied map — plain serde semantics, with
    /// no hidden merge. The one omission that would otherwise loosen enforcement
    /// (a mutating boundary verb) is handled by [`MUTATING_BOUNDARY_VERBS`],
    /// which the derivation consults independently of this map, so dropping an
    /// entry here cannot weaken the derivation.
    pub operation_to_scope: HashMap<String, String>,
}

/// The mutating boundary verbs the derivation's soundness depends on.
///
/// `derive_scope_class` trusts a single recognized boundary segment, so a
/// mutating verb the derivation cannot recognize is not neutral: an id whose
/// other boundary is `read`/`get`/`list` derives that read class unopposed and a
/// read-only token is admitted to a mutating operation. The two boundaries have
/// to disagree for the derivation to refuse — so this list is load-bearing, not
/// decoration.
///
/// It is deliberately NOT merged into `operation_to_scope`. Merging it would have
/// to pick a class name for each verb, and there is no correct one to pick: the
/// literal `write` ignores a deployment that configured a stricter
/// `default_unmapped_scope` (a verbatim map hit never reaches the fallback), and
/// `default_unmapped_scope` itself is wrong in the other direction — a deployment
/// whose fallback IS its read class would make every verb here read-class and
/// turn `list_objects_purge` into an agreement on `read`. Scope class names are
/// opaque strings with no ordering, so no third choice is available either.
///
/// Consulting the list only from the derivation avoids the question entirely: a
/// boundary verb recognized here and absent from the map forces the derivation to
/// refuse, which lands the id on `default_unmapped_scope` — whatever the
/// deployment configured, in either direction. An operator who really means to
/// classify one of these verbs still can, by naming it in `operation_to_scope`;
/// their entry is a map hit and wins.
pub(crate) const MUTATING_BOUNDARY_VERBS: &[&str] = &[
    "write", "delete", "start", "stop", "restart", "create", "update", "patch", "remove", "revoke",
    "remap", "rollback", "purge",
];

impl Default for ScopeEnforcementConfig {
    fn default() -> Self {
        let mut operation_to_scope = HashMap::new();
        operation_to_scope.insert("read".to_owned(), "read".to_owned());
        // `get`/`list` are read-style operations; map them to the `read` scope
        // class so a read-only token can perform them. Without these, they fall
        // back to `default_unmapped_scope` ("write") and a read-only token is
        // wrongly denied a GET/LIST.
        operation_to_scope.insert("get".to_owned(), "read".to_owned());
        operation_to_scope.insert("list".to_owned(), "read".to_owned());
        // The write side is enumerated from `MUTATING_BOUNDARY_VERBS` so the two
        // can never drift apart — a verb added there is automatically classed here
        // too. These entries make the DEFAULT map explicit rather than implied:
        // the platform's own closed verb vocabulary includes bare `write`,
        // `delete`, `start`, `stop` and `restart` as whole operation ids, and they
        // are named here so the shipped behavior is a map hit and not a trip
        // through the fallback. The rest carry no weight for a whole-id lookup —
        // no platform operation is called `create` on its own — and are present so
        // the shipped map spells out the whole mutating vocabulary in one place.
        //
        // A caller-supplied map replaces all of this, and that is safe: the
        // derivation reads `MUTATING_BOUNDARY_VERBS` directly, so an omitted
        // mutating verb still contradicts a read boundary and still lands the id
        // on `default_unmapped_scope`. What an omission costs is only the bare
        // whole-id hit above — `delete` then resolves through the fallback instead
        // of through the map, which is the deployment's own configured answer for
        // an operation it chose not to map.
        for verb in MUTATING_BOUNDARY_VERBS {
            operation_to_scope.insert((*verb).to_owned(), "write".to_owned());
        }

        Self {
            wildcard_scope: "*".to_owned(),
            default_unmapped_scope: "write".to_owned(),
            operation_to_scope,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityDegradationConfig {
    pub max_expansion_ids: usize,
}

impl Default for CapabilityDegradationConfig {
    fn default() -> Self {
        Self {
            max_expansion_ids: 10_000,
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
