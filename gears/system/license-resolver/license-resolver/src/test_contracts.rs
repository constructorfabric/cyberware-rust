//! Licensing contracts standing in for a consuming Gear's published surface.
//!
//! Declared with the real `#[gts_type_schema]` macro rather than hand-written
//! JSON, so tests run against the schema shape a Gear actually registers. These
//! are external GTS types, not domain models, which is why they live outside the
//! domain layer.

use std::sync::Arc;

use gts::{GtsSchema, GtsTypeId};
use license_resolver_sdk::gts::{LicenseResourceV1, LicenseSubjectV1};
use serde_json::Value;
use toolkit_gts::gts_id;
use types_registry_sdk::GtsTypeSchema;

/// Subject contract: `metadata: { category: string }`.
#[toolkit_gts::gts_type_schema(
    dir_path = "schemas",
    base = LicenseSubjectV1,
    type_id = gts_id!("cf.core.lic.subj.v1~test.lic.gateway.user.v1~"),
    description = "test Subject licensing contract",
    properties = "category"
)]
#[derive(Default)]
pub struct TestUserSubjectV1 {
    category: String,
}

/// Resource contract admitting [`TestUserSubjectV1`].
#[toolkit_gts::gts_type_schema(
    dir_path = "schemas",
    base = LicenseResourceV1,
    type_id = gts_id!("cf.core.lic.res.v1~test.lic.gateway.model_usage.v1~"),
    description = "test Resource licensing contract",
    properties = "model_vendor,model_name",
    traits = serde_json::json!({
        "admitted_subjects": [gts_id!("cf.core.lic.subj.v1~test.lic.gateway.user.v1~")]
    })
)]
#[derive(Default)]
pub struct TestModelUsageResourceV1 {
    model_vendor: String,
    model_name: String,
}

/// A second Subject contract, deliberately *not* admitted by
/// [`TestModelUsageResourceV1`].
#[toolkit_gts::gts_type_schema(
    dir_path = "schemas",
    base = LicenseSubjectV1,
    type_id = gts_id!("cf.core.lic.subj.v1~test.lic.gateway.tenant.v1~"),
    description = "test Subject licensing contract that no test Resource admits",
    properties = "tier"
)]
#[derive(Default)]
pub struct TestTenantSubjectV1 {
    tier: String,
}

/// Resource contract that declares no `traits` at all, so trait resolution falls
/// back to the abstract base's `admitted_subjects: []`.
#[toolkit_gts::gts_type_schema(
    dir_path = "schemas",
    base = LicenseResourceV1,
    type_id = gts_id!("cf.core.lic.res.v1~test.lic.gateway.bare.v1~"),
    description = "test Resource contract declaring no admitted subjects",
    properties = "model_name"
)]
#[derive(Default)]
pub struct TestBareResourceV1 {
    model_name: String,
}

pub const SUBJECT_BASE: &str = <LicenseSubjectV1<()> as GtsSchema>::TYPE_ID;
pub const RESOURCE_BASE: &str = <LicenseResourceV1<()> as GtsSchema>::TYPE_ID;
pub const USER_SUBJECT: &str = <TestUserSubjectV1 as GtsSchema>::TYPE_ID;
pub const TENANT_SUBJECT: &str = <TestTenantSubjectV1 as GtsSchema>::TYPE_ID;
pub const MODEL_USAGE_RESOURCE: &str = <TestModelUsageResourceV1 as GtsSchema>::TYPE_ID;
pub const BARE_RESOURCE: &str = <TestBareResourceV1 as GtsSchema>::TYPE_ID;

fn schema_of(type_id: &str, body: Value, parent: Option<Arc<GtsTypeSchema>>) -> GtsTypeSchema {
    GtsTypeSchema::try_new(GtsTypeId::new(type_id), body, None, parent)
        .expect("test contract type-schema is well formed")
}

/// The abstract Subject licensing base, exactly as the SDK publishes it.
#[must_use]
pub fn subject_base_schema() -> GtsTypeSchema {
    schema_of(
        SUBJECT_BASE,
        <LicenseSubjectV1<()> as GtsSchema>::gts_schema_with_refs(),
        None,
    )
}

/// The abstract Resource licensing base, exactly as the SDK publishes it.
#[must_use]
pub fn resource_base_schema() -> GtsTypeSchema {
    schema_of(
        RESOURCE_BASE,
        <LicenseResourceV1<()> as GtsSchema>::gts_schema_with_refs(),
        None,
    )
}

/// A derived contract with its licensing base linked as parent, so `ancestors`
/// and `effective_*` observe the whole chain the registry would resolve.
fn derived_schema<T: GtsSchema>(base: GtsTypeSchema) -> GtsTypeSchema {
    schema_of(T::TYPE_ID, T::gts_schema_with_refs(), Some(Arc::new(base)))
}

#[must_use]
pub fn user_subject_schema() -> GtsTypeSchema {
    derived_schema::<TestUserSubjectV1>(subject_base_schema())
}

#[must_use]
pub fn tenant_subject_schema() -> GtsTypeSchema {
    derived_schema::<TestTenantSubjectV1>(subject_base_schema())
}

#[must_use]
pub fn model_usage_schema() -> GtsTypeSchema {
    derived_schema::<TestModelUsageResourceV1>(resource_base_schema())
}

#[must_use]
pub fn bare_resource_schema() -> GtsTypeSchema {
    derived_schema::<TestBareResourceV1>(resource_base_schema())
}
