//! Shared fixtures for the plugin's unit tests.

#![allow(clippy::missing_panics_doc)]

use gts::GtsTypeId;
use license_resolver_sdk::{LicenseCheckContext, LicenseCheckRequest, Resource, Subject};
use serde_json::{Map, Value, json};
use toolkit_gts::gts_id;
use uuid::Uuid;

use crate::config::GrantRule;

pub const RESOURCE_TYPE: &str = gts_id!("cf.core.lic.res.v1~test.lic.gateway.model_usage.v1~");
pub const SUBJECT_TYPE: &str = gts_id!("cf.core.lic.subj.v1~test.lic.gateway.user.v1~");
pub const OTHER_SUBJECT_TYPE: &str = gts_id!("cf.core.lic.subj.v1~test.lic.gateway.tenant.v1~");
pub const RESOURCE_BASE: &str = gts_id!("cf.core.lic.res.v1~");

#[must_use]
pub fn tenant() -> Uuid {
    Uuid::from_u128(0x5eed_0000_0000_0000_0000_0000_0000_0001)
}

#[must_use]
pub fn other_tenant() -> Uuid {
    Uuid::from_u128(0x5eed_0000_0000_0000_0000_0000_0000_0002)
}

fn metadata(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("contract metadata must be a JSON object, got {other}"),
    }
}

#[must_use]
pub fn subject(id: Option<&str>, meta: Value) -> Subject {
    Subject {
        gts_type: GtsTypeId::new(SUBJECT_TYPE),
        id: id.map(ToOwned::to_owned),
        metadata: metadata(meta),
    }
}

#[must_use]
pub fn resource(id: Option<&str>, meta: Value) -> Resource {
    Resource {
        gts_type: GtsTypeId::new(RESOURCE_TYPE),
        id: id.map(ToOwned::to_owned),
        metadata: metadata(meta),
    }
}

#[must_use]
pub fn request_for(subject: Subject, resource: Resource, tenant_id: Uuid) -> LicenseCheckRequest {
    let context = LicenseCheckContext::builder()
        .tenant_id(tenant_id)
        .build()
        .expect("tenant is set");
    LicenseCheckRequest::new(subject, resource, context)
}

/// The check the fixtures are built around: a named user against a named model.
#[must_use]
pub fn request() -> LicenseCheckRequest {
    request_for(
        subject(Some("acme-admin"), json!({ "category": "internal" })),
        resource(
            Some("gpt-4o"),
            json!({ "model_vendor": "openai", "model_name": "gpt-4o" }),
        ),
        tenant(),
    )
}

/// A rule constraining nothing beyond the two contract types.
#[must_use]
pub fn rule() -> GrantRule {
    GrantRule {
        resource_type: RESOURCE_TYPE.to_owned(),
        resource_id: None,
        subject_type: SUBJECT_TYPE.to_owned(),
        subject_id: None,
        tenant_id: None,
        resource_metadata: Map::new(),
        subject_metadata: Map::new(),
    }
}
