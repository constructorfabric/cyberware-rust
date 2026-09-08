//! GTS type definitions owned by the license resolver.
//!
//! Two concerns live here:
//!
//! 1. The **plugin spec** [`LicenseResolverPluginSpecV1`] — a derived toolkit
//!    plugin type the gateway discovers backend plugins with (vendor +
//!    priority).
//! 2. The **licensing base types** [`LicenseSubjectV1`] (`gts.cf.core.lic.subj.v1~`)
//!    and [`LicenseResourceV1`] (`gts.cf.core.lic.res.v1~`) — the abstract bases
//!    consuming Gears derive their concrete Subject / Resource contract types
//!    from. Because every licensing contract derives from these, querying the
//!    registry for everything under them yields a platform's entire licensing
//!    surface, with no resolver API involved.
//!
//! The base types are **abstract** (`x-gts-abstract`) — only derived contract
//! types are instantiable. A derived Resource type overrides the
//! [`admitted_subjects`](ResourceTraits::admitted_subjects) trait to declare
//! which Subject contract types it may be checked against.

use schemars::JsonSchema;
use toolkit_gts::{GtsTraitsSchema, PluginV1, gts_id, gts_type_schema};

/// GTS type definition for license resolver plugin instances.
///
/// # Instance ID Format
///
/// ```text
/// gts.cf.toolkit.plugins.plugin.v1~<vendor>.<package>.license_resolver.plugin.v1~
/// ```
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.license_resolver.plugin.v1~"),
    description = "License Resolver plugin specification",
    properties = "",
)]
pub struct LicenseResolverPluginSpecV1;

// Narrows the emitted `type` property from `GtsTypeId`'s own open
// `x-gts-ref: "gts.*"` to the licensing base the id must derive from. The field
// must stay a `GtsTypeId`, so the override goes through schemars.
fn contract_type_schema(
    generator: &mut schemars::SchemaGenerator,
    base_type_id: &str,
) -> schemars::Schema {
    let mut schema = <gts::GtsTypeId as JsonSchema>::json_schema(generator);
    schema.insert("x-gts-ref".to_owned(), base_type_id.into());
    schema
}

fn subject_contract_type_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    contract_type_schema(generator, gts_id!("cf.core.lic.subj.v1~"))
}

fn resource_contract_type_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    contract_type_schema(generator, gts_id!("cf.core.lic.res.v1~"))
}

/// Base **Subject** licensing type — `gts.cf.core.lic.subj.v1~`.
///
/// Abstract: consuming Gears register derived Subject contract types (e.g.
/// `…subj.v1~<gear>.user.v1~`) that refine the `metadata` schema. This module
/// owns only the base, and it pins the wire shape shared by every derived
/// contract (see [`crate::models::Subject`], the runtime value object whose
/// schema this must match).
#[gts_type_schema(
    dir_path = "schemas",
    base = true,
    type_id = gts_id!("cf.core.lic.subj.v1~"),
    description = "Base Subject licensing contract type. `type` names the derived \
                   Subject contract; `id` is an optional instance id; derived \
                   contracts refine the `metadata` schema. The resolver validates \
                   requests against the derived contract before delegation.",
    properties = "gts_type,id,metadata",
    gts_abstract = true
)]
pub struct LicenseSubjectV1<M: gts::GtsSchema> {
    /// The derived Subject contract type this object instantiates — what the resolver resolves the schema from. Wire key `type`.
    #[serde(rename = "type")]
    #[schemars(schema_with = "subject_contract_type_schema")]
    pub gts_type: gts::GtsTypeId,
    /// Optional instance id — a well-known name or a UUID; absent for a type-level subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 255))]
    pub id: Option<String>,
    /// Metadata extension point: a derived Subject contract type fills `M` with
    /// its own metadata struct, refining the schema inside `metadata` without
    /// adding any licensing property at the top level.
    pub metadata: M,
}

// A single admitted Subject contract type. `x-gts-ref` validates the shape of
// the id, not that the type is registered.
#[allow(dead_code)]
#[derive(serde::Serialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent, extend("x-gts-ref" = gts_id!("cf.core.lic.subj.v1~")))]
pub struct SubjectTypeRef(pub String);

// Behavioral traits for `gts.cf.core.lic.res.v1~` — emitted as the base Resource
// type's `x-gts-traits-schema`. A derived Resource type that declares no `traits`
// inherits this base's `[]` and so denies every check.
#[derive(JsonSchema, serde::Serialize, GtsTraitsSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceTraits {
    /// GTS type ids of the Subject contract types this Resource may be checked against. A derived Resource type MUST declare it: an empty list admits no subject, so every check against the contract is rejected as inadmissible. Widening the list is non-breaking; narrowing it requires a new contract version.
    #[serde(default)]
    pub admitted_subjects: Vec<SubjectTypeRef>,
}

/// Base **Resource** licensing type — `gts.cf.core.lic.res.v1~`.
///
/// Abstract: consuming Gears register derived Resource contract types (e.g.
/// `…res.v1~<gear>.model_usage.v1~`) that refine the `metadata` schema and
/// declare their [`admitted_subjects`](ResourceTraits::admitted_subjects). This
/// module owns only the base, and it pins the wire shape shared by every derived
/// contract (see [`crate::models::Resource`], the runtime value object whose
/// schema this must match).
#[gts_type_schema(
    dir_path = "schemas",
    base = true,
    type_id = gts_id!("cf.core.lic.res.v1~"),
    description = "Base Resource licensing contract type. `type` names the derived \
                   Resource contract; `id` is an optional instance id (absent = \
                   whole-type check); derived contracts refine the `metadata` schema \
                   and declare admitted_subjects. The resolver validates requests \
                   against the derived contract before delegation.",
    properties = "gts_type,id,metadata",
    traits_schema = inline(ResourceTraits),
    // Abstract bases MUST carry trait defaults: the GTS store rejects an
    // `x-gts-traits-schema` with no values in the derivation chain (even for
    // abstract types), which would fail types-registry ready-commit. Mirrors
    // the trait-schema `default`s.
    traits = serde_json::json!({ "admitted_subjects": [] }),
    gts_abstract = true
)]
pub struct LicenseResourceV1<M: gts::GtsSchema> {
    /// The derived Resource contract type this object instantiates — what the resolver resolves the schema and `admitted_subjects` from. Wire key `type`.
    #[serde(rename = "type")]
    #[schemars(schema_with = "resource_contract_type_schema")]
    pub gts_type: gts::GtsTypeId,
    /// Optional instance id — a well-known name or a UUID; absent for a whole-type check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 255))]
    pub id: Option<String>,
    /// Metadata extension point: a derived Resource contract type fills `M` with
    /// its own metadata struct, refining the schema inside `metadata` without
    /// adding any licensing property at the top level.
    pub metadata: M,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gts_tests.rs"]
mod gts_tests;
