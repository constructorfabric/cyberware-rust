//! Permission matching engine — pure-domain primitives composed by the
//! Permission Evaluator across roles.
//!
//! Entry points (all `pub(crate)`):
//! * [`matches_operation`] — exact case-sensitive equality plus the
//!   single-character `*` wildcard.
//! * [`matches_target_type`] — exact GTS type equality plus the GTS OP#4
//!   family wildcard `prefix.*`. **No bare `*` branch** — the JSON Schema
//!   disallows bare `*` for `target_type`.
//! * [`validate_permission_rule`] — write-time validation with
//!   field-tagged errors.
//! * [`is_permission_allowed`] — single-role evaluator returning the
//!   tri-state [`PermissionMatchResult`].
//!
//! ## Canonical contract: schema / validator / matcher
//!
//! Three artefacts encode "what is a valid permission rule":
//!
//! 1. **Schema**:
//!    `schemas/role_definition.v1.schema.json` —
//!    `$defs.operation` is the canonical `operation` regex (mirrored verbatim
//!    below); `$defs.gts_type_or_wildcard` is a *coarse advisory* shape only
//!    (a regex cannot express the chained-`~` GTS grammar).
//! 2. **Validator (server-side, authoritative)**:
//!    [`validate_permission_rule`] gates every role-definition write.
//!    `operation` mirrors the schema regex verbatim; `target_type` delegates
//!    to the canonical GTS parser ([`gts::GtsId::is_valid`]) — the parser, not
//!    the schema, is the source of truth for the type grammar.
//! 3. **Matcher (runtime)**: [`matches_operation`] /
//!    [`matches_target_type`] see only already-validated rules, so they
//!    skip defence-in-depth re-validation on the hot path.
//!
//! Drift between the `operation` schema regex and its validator is a bug; the
//! `target_type` grammar is owned by the `gts` crate.
//!
//! ## Boundary against `rbac-sdk::PermissionResult`
//!
//! [`PermissionMatchResult`] is **intra-role state** with three variants
//! so the Permission Evaluator can distinguish "this role actively
//! excludes the request" from "this role is silent". It MUST stay
//! `pub(crate)` — promoting it to the SDK would lock an internal helper
//! into the ABI.
//!
//! ## Performance
//!
//! All four functions are synchronous, allocation-free on the failure
//! path, and `O(n)` in the input length / rule count. NFR target —
//! `is_permission_allowed` p95 ≤ 1 ms — is met for representative role
//! sizes (≤100 rules per role).

use rbac_sdk::models::{PermissionRule, RolePolicy};
use toolkit_macros::domain_model;

/// Single-character global wildcard for the `operation` field and the
/// trailing segment of `target_type`.
const WILDCARD: &str = "*";

/// Maximum byte length for `PermissionRule::operation`. 64 bytes leaves
/// headroom for long compound verbs while bounding the matcher's scan
/// cost. Measured with `str::len` (UTF-8 bytes).
pub const MAX_OPERATION_LEN: usize = 64;

/// Maximum byte length for `PermissionRule::target_type`. GTS
/// identifiers plus family wildcards need more headroom than operations;
/// 256 bytes accommodates several future versions and deeper namespaces.
/// Measured with `str::len` (UTF-8 bytes).
pub const MAX_TARGET_TYPE_LEN: usize = 256;

/// Identifier for a single field on `PermissionRule` reported by
/// [`ValidationError`]. The `Display` impl renders the `snake_case`
/// field name verbatim so the REST error mapper can place it into
/// RFC 9457 `errors[].field` without translation. Closed enum so a
/// future third field on `PermissionRule` is a compile-time miss in
/// every match site.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionRuleField {
    /// `PermissionRule::operation`.
    Operation,
    /// `PermissionRule::target_type`.
    TargetType,
}

impl std::fmt::Display for PermissionRuleField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Operation => "operation",
            Self::TargetType => "target_type",
        };
        f.write_str(name)
    }
}

/// Domain-internal error returned by [`validate_permission_rule`].
/// Variants carry a [`PermissionRuleField`] discriminator so the
/// API-layer mapper can emit one RFC 9457 `errors[]` entry per failing
/// field without parsing strings.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// The field's value was the empty string.
    EmptyField {
        /// Which `PermissionRule` field was empty.
        field: PermissionRuleField,
    },
    /// The field's UTF-8 byte length exceeded the configured maximum.
    FieldTooLong {
        /// Which `PermissionRule` field exceeded its bound.
        field: PermissionRuleField,
        /// Configured upper bound, in bytes.
        max: usize,
        /// Observed length of the field, in bytes.
        actual: usize,
    },
    /// The field's value did not match the canonical JSON Schema shape
    /// (`schemas/role_definition.v1.schema.json` `$defs.operation` /
    /// `$defs.gts_type_or_wildcard`). Emitted after non-empty / length
    /// checks so the resulting error explains *what* shape is required.
    InvalidShape {
        /// Which `PermissionRule` field violated the schema shape.
        field: PermissionRuleField,
    },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyField { field } => {
                write!(f, "permission rule field `{field}` must be non-empty")
            }
            Self::FieldTooLong { field, max, actual } => write!(
                f,
                "permission rule field `{field}` length {actual} bytes exceeds maximum {max} bytes",
            ),
            Self::InvalidShape { field } => match field {
                PermissionRuleField::Operation => f.write_str(
                    "permission rule field `operation` must match `^(\\*|[a-z][a-z0-9_]*)$` \
                     (lowercase identifier or the single wildcard `*`)",
                ),
                PermissionRuleField::TargetType => f.write_str(
                    "permission rule field `target_type` must be a canonical GTS type id \
                     (trailing `~`, possibly chained, e.g. \
                     `gts.cf.resources.rms.resource.v1~x.storage.s3.bucket.v1~`) or a family \
                     wildcard `prefix.*` (e.g. `gts.cf.*`); a bare instance id, bare `gts.*`, \
                     or a value the GTS grammar rejects is not allowed",
                ),
            },
        }
    }
}

impl std::error::Error for ValidationError {}

/// Tri-state outcome of [`is_permission_allowed`] for a single role.
/// See module docs for the boundary against
/// `rbac_sdk::models::PermissionResult`.
///
/// ## Mapping to the Permission Evaluator
///
/// * `Allowed` → include `matching_rule` in the `EffectivePermission`
///   payload.
/// * `ExcludedByNotPermission` → if no other role returns `Allowed`,
///   map to `DenyReason::NotPermissionExclusion`.
/// * `NoMatch` → silent role; if every role returns `NoMatch`, map to
///   `DenyReason::NoMatchingPermission`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionMatchResult {
    /// An `Allow` rule matched and no `Deny` rule excluded the request
    /// within the same role (first-match-wins, `Deny` short-circuits).
    Allowed {
        /// The `Allow` rule that produced the match.
        matching_rule: PermissionRule,
    },
    /// A `Deny` rule matched, overriding any `Allow` grant within the
    /// same role. The variant name preserves the wire-shape mapping to
    /// [`rbac_sdk::models::DenyReason::NotPermissionExclusion`].
    ExcludedByNotPermission {
        /// The `Deny` rule that produced the exclusion.
        matching_rule: PermissionRule,
    },
    /// No rule matched the request.
    NoMatch,
}

/// Returns `true` when `rule_op` matches `requested_op`.
///
/// * If `rule_op == "*"` → match every requested op (no partial wildcard
///   forms: `"read*"` is the literal string).
/// * Otherwise byte-for-byte case-sensitive equality.
pub fn matches_operation(rule_op: &str, requested_op: &str) -> bool {
    if rule_op == WILDCARD {
        return true;
    }
    rule_op == requested_op
}

/// Returns `true` when `rule_target` matches `requested_type`.
///
/// Aligned with `schemas/role_definition.v1.schema.json`
/// `$defs.gts_type_or_wildcard`:
///
/// 1. Empty `requested_type` → `false` (defence in depth).
/// 2. Exact equality → `true`.
/// 3. GTS OP#4 family wildcard `prefix.*` → strip the trailing `'*'`
///    keeping the `'.'`, then `requested_type.starts_with(prefix)`. The
///    retained dot prevents `"compute.*"` from matching `"computer.…"`.
/// 4. Otherwise → `false`.
///
/// **No bare `"*"` branch.** Operators express "every resource type
/// under a namespace" as `prefix.*` (e.g. `gts.cf.*`). All comparisons
/// are plain string operations — no regex, allocation-free, `O(n)`.
pub fn matches_target_type(rule_target: &str, requested_type: &str) -> bool {
    if requested_type.is_empty() {
        return false;
    }
    if rule_target == requested_type {
        return true;
    }
    // GTS OP#4 family wildcard (`prefix.*`). Strip only the trailing
    // `'*'`; the retained `'.'` is the namespace separator that prevents
    // `"compute.*"` (prefix `"compute."`) from matching `"computer.…"`.
    if let Some(prefix) = rule_target.strip_suffix('*')
        && prefix.ends_with('.')
    {
        return requested_type.starts_with(prefix);
    }
    // Bare `"*"` lands here and returns `false`.
    false
}

/// Validate a [`PermissionRule`] at write time.
///
/// Checks run in order; the first failure short-circuits:
///
/// 1. `operation` non-empty.
/// 2. `target_type` non-empty.
/// 3. `operation` byte length ≤ `MAX_OPERATION_LEN`.
/// 4. `target_type` byte length ≤ `MAX_TARGET_TYPE_LEN`.
/// 5. `operation` matches `^(\*|[a-z][a-z0-9_]*)$`
///    (`$defs.operation`).
/// 6. `target_type` is a canonical GTS type id (trailing `~`, possibly
///    chained) or a family wildcard `prefix.*`, per
///    [`is_valid_target_type_shape`] — the GTS grammar is owned by
///    [`gts::GtsId::is_valid`], not a regex. Bare `"*"` / `gts.*` is rejected.
///
/// Lengths are measured with `str::len` (UTF-8 bytes).
///
/// ## Errors
///
/// Returns [`ValidationError::EmptyField`], [`ValidationError::FieldTooLong`],
/// or [`ValidationError::InvalidShape`], each carrying a
/// [`PermissionRuleField`] discriminator.
///
/// ## Source of truth
///
/// Canonical rule shape lives in
/// `schemas/role_definition.v1.schema.json`.
/// This validator and the matcher are kept in lock-step with that schema.
pub fn validate_permission_rule(rule: &PermissionRule) -> Result<(), ValidationError> {
    if rule.operation.is_empty() {
        return Err(ValidationError::EmptyField {
            field: PermissionRuleField::Operation,
        });
    }
    if rule.target_type.is_empty() {
        return Err(ValidationError::EmptyField {
            field: PermissionRuleField::TargetType,
        });
    }
    let op_len = rule.operation.len();
    if op_len > MAX_OPERATION_LEN {
        return Err(ValidationError::FieldTooLong {
            field: PermissionRuleField::Operation,
            max: MAX_OPERATION_LEN,
            actual: op_len,
        });
    }
    let tt_len = rule.target_type.len();
    if tt_len > MAX_TARGET_TYPE_LEN {
        return Err(ValidationError::FieldTooLong {
            field: PermissionRuleField::TargetType,
            max: MAX_TARGET_TYPE_LEN,
            actual: tt_len,
        });
    }
    if !is_valid_operation_shape(&rule.operation) {
        return Err(ValidationError::InvalidShape {
            field: PermissionRuleField::Operation,
        });
    }
    if !is_valid_target_type_shape(&rule.target_type) {
        return Err(ValidationError::InvalidShape {
            field: PermissionRuleField::TargetType,
        });
    }
    Ok(())
}

/// Returns `true` when `s` matches the JSON-Schema `$defs.operation`
/// regex `^(\*|[a-z][a-z0-9_]*)$`. MUST stay equivalent to the schema.
fn is_valid_operation_shape(s: &str) -> bool {
    if s == WILDCARD {
        return true;
    }
    let bytes = s.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    rest.iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Returns `true` when `s` is a valid RBAC `target_type`: a canonical GTS
/// **type id** (trailing `~`, possibly chained) or a GTS family **wildcard**
/// (`prefix.*` with at least one head segment — bare `gts.*` is rejected).
///
/// The GTS grammar itself (segments, `_` placeholder, `~`-chaining, version
/// tokens, segment/length caps) is delegated to the canonical parsers — the
/// single source of truth shared platform-wide, so this validator can never
/// drift from the real grammar. Wildcards are validated with
/// [`gts::GtsIdPattern::is_valid`] (a concrete [`gts::GtsId`] never carries a
/// `*`), concrete type ids with [`gts::GtsId::is_valid`]. The suffix guard
/// encodes the RBAC-specific policy the parser cannot know: a `target_type` is
/// a type-or-wildcard, never a concrete instance id, and never the
/// match-everything bare `gts.*`.
fn is_valid_target_type_shape(s: &str) -> bool {
    if let Some(prefix) = s.strip_suffix(".*") {
        // Family wildcard: reject bare `gts.*` (no head segment); the pattern
        // parser validates the rest. `GtsId` rejects `*`, so the wildcard form
        // goes through `GtsIdPattern::is_valid`.
        return prefix != "gts" && ::gts::GtsIdPattern::is_valid(s);
    }
    // Concrete: a TYPE id only (trailing `~`); instance ids are not grantable.
    s.ends_with('~') && ::gts::GtsId::is_valid(s)
}

/// Evaluate whether a single role grants, excludes, or is silent about
/// an `(operation, target_type)` request.
///
/// **Precedence within a role**: `Deny` (any matching `not_permissions`
/// rule) always wins over `Allow`. The first matching `Allow` is the
/// one reported if no `Deny` matches. Cross-role union is the Permission
/// Evaluator's job.
///
/// Performance: `O(|not_permissions| + |permissions|)`; the deny scan
/// short-circuits on the first match.
pub fn is_permission_allowed(
    policy: &RolePolicy,
    requested_op: &str,
    requested_type: &str,
) -> PermissionMatchResult {
    // Deny pass first: any matching not_permission short-circuits.
    for rule in &policy.not_permissions {
        if matches_operation(&rule.operation, requested_op)
            && matches_target_type(&rule.target_type, requested_type)
        {
            return PermissionMatchResult::ExcludedByNotPermission {
                matching_rule: rule.clone(),
            };
        }
    }
    // Allow pass: first match wins.
    for rule in &policy.permissions {
        if matches_operation(&rule.operation, requested_op)
            && matches_target_type(&rule.target_type, requested_type)
        {
            return PermissionMatchResult::Allowed {
                matching_rule: rule.clone(),
            };
        }
    }
    PermissionMatchResult::NoMatch
}

#[cfg(test)]
#[path = "permission_matcher_tests.rs"]
mod permission_matcher_tests;
