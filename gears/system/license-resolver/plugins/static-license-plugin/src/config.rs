//! Configuration for the static license resolver plugin.

use anyhow::{Context, bail};
use gts::{GtsSchema, GtsTypeId};
use license_resolver_sdk::LicenseCheckRequest;
use license_resolver_sdk::gts::{LicenseResourceV1, LicenseSubjectV1};
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

/// Plugin configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticLicensePluginConfig {
    /// Vendor name for GTS instance registration.
    pub vendor: String,

    /// Plugin priority (lower = higher priority).
    pub priority: i16,

    /// Grant rules. Deny by default: an empty list grants nothing.
    pub grants: Vec<GrantRule>,
}

impl Default for StaticLicensePluginConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 100,
            grants: Vec::new(),
        }
    }
}

impl StaticLicensePluginConfig {
    /// Validates every configured rule.
    ///
    /// A rule naming a type that could never appear in a check would silently
    /// never match, which reads as "not licensed" — the deny that hides a typo.
    /// Failing at startup keeps that distinguishable.
    ///
    /// # Errors
    ///
    /// Returns an error if any rule names a malformed or wrongly-based contract
    /// type.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (index, rule) in self.grants.iter().enumerate() {
            rule.validate().with_context(|| {
                format!("static-license-plugin: grant rule #{index} is invalid")
            })?;
        }
        Ok(())
    }
}

/// One grant: the check is granted when every field set here matches the
/// request. A field left unset constrains nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRule {
    /// Resource contract type the grant covers.
    pub resource_type: String,

    /// Resource instance the grant covers. Unset licenses the whole type, which
    /// also answers a check naming one instance of it; set, it answers only that
    /// instance and never a whole-type check.
    #[serde(default)]
    pub resource_id: Option<String>,

    /// Subject contract type the grant covers.
    pub subject_type: String,

    /// Subject instance the grant covers. Unset covers every subject of the type.
    #[serde(default)]
    pub subject_id: Option<String>,

    /// Tenant the grant is scoped to. Unset covers every tenant in this
    /// deployment.
    #[serde(default)]
    pub tenant_id: Option<Uuid>,

    /// Resource `metadata` properties that must match exactly. This is where
    /// attribute-based licensing lives — "licensed, but only for this vendor's
    /// models". Properties not named here are unconstrained.
    #[serde(default)]
    pub resource_metadata: Map<String, Value>,

    /// Subject `metadata` properties that must match exactly, e.g. licensing a
    /// resource only to subjects of a given category.
    #[serde(default)]
    pub subject_metadata: Map<String, Value>,
}

impl GrantRule {
    /// Whether this rule answers the given check.
    #[must_use]
    pub fn matches(&self, request: &LicenseCheckRequest) -> bool {
        self.resource_type == request.resource.gts_type.as_ref()
            && self.subject_type == request.subject.gts_type.as_ref()
            && matches_instance(self.resource_id.as_deref(), request.resource.id.as_deref())
            && matches_instance(self.subject_id.as_deref(), request.subject.id.as_deref())
            && self
                .tenant_id
                .is_none_or(|tenant_id| tenant_id == request.context.tenant_id)
            && contains_all(&request.resource.metadata, &self.resource_metadata)
            && contains_all(&request.subject.metadata, &self.subject_metadata)
    }

    fn validate(&self) -> anyhow::Result<()> {
        validate_contract_type(
            &self.resource_type,
            <LicenseResourceV1<()> as GtsSchema>::TYPE_ID,
            "resource_type",
        )?;
        validate_contract_type(
            &self.subject_type,
            <LicenseSubjectV1<()> as GtsSchema>::TYPE_ID,
            "subject_type",
        )
    }
}

/// An unset id in a rule covers any instance; a set one must be exactly the
/// instance the check names, so a grant for one instance never answers a
/// whole-type check.
fn matches_instance(rule_id: Option<&str>, request_id: Option<&str>) -> bool {
    rule_id.is_none_or(|expected| request_id == Some(expected))
}

/// Every property of `expected` is present in `actual` with an equal value.
fn contains_all(actual: &Map<String, Value>, expected: &Map<String, Value>) -> bool {
    expected
        .iter()
        .all(|(key, value)| actual.get(key) == Some(value))
}

fn validate_contract_type(type_id: &str, base: &str, field: &str) -> anyhow::Result<()> {
    GtsTypeId::try_new(type_id)
        .map_err(|e| anyhow::anyhow!("{field} '{type_id}' is not a valid GTS type id: {e}"))?;

    // A licensing base id ends with `~`, so a chain prefix is also a byte
    // prefix and this cannot match a merely similarly-named type.
    if !type_id.starts_with(base) || type_id.len() <= base.len() {
        bail!("{field} '{type_id}' does not derive from the licensing base '{base}'");
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
