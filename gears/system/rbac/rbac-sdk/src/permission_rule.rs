//! Single `{ operation, target_type }` rule + the action vocabulary.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Built-in permission action verbs known at compile time.
///
/// `PermissionRule.operation` carries the wire form as a free-form `String`
/// because plugins may register additional verbs (e.g. `start`, `stop`,
/// `scale`) that the SDK cannot enumerate. This enum names the verbs the
/// rbac module itself authors and matches against, so call sites that
/// reference them get a `&'static str` from [`Self::as_str`] instead of a
/// raw literal.
///
/// `#[non_exhaustive]` — external `match` arms MUST end with `_ =>`.
///
/// `Serialize`/`Deserialize` are hand-written below rather than derived.
/// The derive emitted the *variant* names (`"Read"`, `"Wildcard"`), while
/// [`Self::as_str`], [`std::fmt::Display`], [`std::str::FromStr`] and every
/// `PermissionRule.operation` value use the canonical verbs (`"read"`,
/// `"*"`) — one public type with two contradictory string encodings. No
/// field in this workspace serializes an `Action` (every call site goes
/// through `as_str`), so nothing depended on the variant-name form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Action {
    /// Read / list operations.
    Read,
    /// Write / create / update operations.
    Write,
    /// Delete operations.
    Delete,
    /// Wildcard matcher — matches every operation in a `PermissionRule`.
    Wildcard,
}

impl Action {
    /// Canonical wire-form verb. `const fn` so it can be used in `const`
    /// contexts such as the built-in role catalog.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Delete => "delete",
            Self::Wildcard => "*",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).as_str())
    }
}

/// Error returned by `Action::from_str` when the input does not match a
/// known verb. Carries the offending value verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAction(pub String);

impl fmt::Display for UnknownAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown action: '{}'", self.0.escape_debug())
    }
}

impl std::error::Error for UnknownAction {}

impl Serialize for Action {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for Action {
    type Err = UnknownAction;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "delete" => Ok(Self::Delete),
            "*" => Ok(Self::Wildcard),
            other => Err(UnknownAction(other.to_owned())),
        }
    }
}

/// Single `{ operation, target_type }` rule inside
/// [`crate::role_definition::RoleDefinition::permissions`] or
/// [`crate::role_definition::RoleDefinition::not_permissions`].
///
/// The rule's class (Allow / Deny) is encoded by which array it lives in.
/// `#[non_exhaustive]` — construct via [`PermissionRule::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PermissionRule {
    /// Short operation verb (e.g. `read`, `write`, `delete`, `start`). `*`
    /// matches any operation. Stored as `String` to remain forward-
    /// compatible with plugin-registered verbs that fall outside the
    /// rbac module's own [`Action`] enum.
    pub operation: String,
    /// GTS target type or wildcard family (e.g. `gts.cf.resources.compute.*`).
    pub target_type: String,
}

impl PermissionRule {
    /// Construct a [`PermissionRule`] from an operation verb and a GTS
    /// target type. Allow/Deny is encoded by which array the rule is placed in.
    #[must_use]
    pub fn new(operation: impl Into<String>, target_type: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            target_type: target_type.into(),
        }
    }

    /// Construct a [`PermissionRule`] from a typed [`Action`] and a target
    /// type. Convenience wrapper around [`Self::new`] that pins the verb to
    /// an enum at the call site.
    #[must_use]
    pub fn with_action(action: Action, target_type: impl Into<String>) -> Self {
        Self::new(action.as_str(), target_type)
    }
}

#[cfg(test)]
#[path = "permission_rule_tests.rs"]
mod permission_rule_tests;
