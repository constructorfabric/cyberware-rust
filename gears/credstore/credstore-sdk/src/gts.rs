//! GTS (Global Type System) declarations for the credstore SDK.
//!
//! Credstore type/resource ids follow the platform convention
//! `gts.cf.core.credstore.<resource>.v1~`: the trailing `~` terminates the type
//! URI and `.v1` is the schema version. Plugin specs nest under the toolkit base,
//! `gts.cf.toolkit.plugins.plugin.v1~cf.core.credstore.<spec>.v1~`.
//!
//! Each `#[gts_type_schema(...)]`-annotated struct submits its schema to the
//! link-time inventory the types-registry loads at boot. Registration is what
//! makes a resource type authorizable: RBAC validates a role's `target_type`
//! against the registry and the PDP compiles a tenant-scoped grant into
//! tenant constraints on `owner_tenant_id`. The same string is the
//! single source of truth for the canonical-error `resource_type` — see
//! [`CREDENTIAL_RESOURCE_TYPE`], pinned to the impl crate's `#[resource_error(...)]`
//! marker by a unit test.
//!
//! **Renamed by ADR-0004.** The base type shipped as
//! `gts.cf.core.credstore.secret.v1~`; it is now
//! `gts.cf.core.credstore.credential.v1~`, so the type, the `credentials`
//! collection, the `Credential`/`Secret` schemas, the PDP actions and the SDK's
//! `get`/`get_secret` all share one noun for the entity (`credential`) and one
//! for the payload (`secret`).

use toolkit::gts::PluginV1;
use toolkit_gts::{gts_id, gts_type_schema};

/// GTS resource-type id for the credstore **credential** base type — the
/// single source of truth for this string. Mirrored by [`CredentialV1`]'s
/// `type_id` and the `#[resource_error(...)]` marker in
/// `infra::sdk_error_mapping` (a unit test there pins that marker to this
/// constant). Enforcement authorizes against the credential's full
/// *concrete* type, not this base: the impl crate's
/// `domain::authz::credential_type_resource(gts_id)` builds the per-operation
/// PEP `ResourceType` from the type resolved out of the types-registry, and
/// the resolver checks a type descends from this base id.
pub const CREDENTIAL_RESOURCE_TYPE: &str = gts_id!("cf.core.credstore.credential.v1~");

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.credstore.plugin.v1~"),
    description = "CredStore plugin specification",
    properties = "",
)]
pub struct CredStorePluginSpecV1;

/// GTS type-schema for the credstore **credential** resource.
///
/// This is the base of the PEP `ResourceType`s credstore enforces on: every
/// operation authorizes against the credential's full concrete type via the
/// impl crate's `domain::authz::credential_type_resource(gts_id)`, where
/// `gts_id` is resolved from the types-registry and checked to descend from
/// this base.
/// Registering it (and its derived types) with the types-registry is what
/// makes the resource types authorizable: the RBAC role-definitions API
/// validates `target_type` against the registry, and the authz-resolver PDP
/// compiles a tenant-scoped grant into tenant constraints on
/// `owner_tenant_id`. Without this registration every credstore operation is
/// denied (403) because no role can name the type.
///
/// Modelled as a GTS **type base** (mirroring model-registry's `ModelInfoV1`):
/// the derived credential types (`GenericSecretV1`, `ApiKeySecretV1`, …) chain
/// off it via `#[gts_type_schema(base = CredentialV1, …)]`, so the macro emits
/// and registers their schemas. The base carries only the contract-mandated
/// fields — a `gts_type` discriminator and one generic `payload` field —
/// because a credential *type* has no payload: types differ purely by their
/// `x-gts-traits`, so `payload` is always `()`. Authorization itself only
/// needs the type *id* known to the registry (RBAC `target_type` validation)
/// and the PDP; the tenant-scope binding comes from the impl crate's
/// `ResourceType` (`OWNER_TENANT_ID`), not from the schema body.
#[derive(Debug)]
#[gts_type_schema(
    dir_path = "schemas",
    type_id = gts_id!("cf.core.credstore.credential.v1~"),
    description = "CredStore credential resource — tenant-scoped, RBAC/PEP protected",
    properties = "gts_type",
    traits_schema = inline(SecretTypeTraits),
    // `x-gts-traits` defaults for the base type itself: generic semantics (all
    // sharing modes, opaque value). Kept although the type is abstract — a
    // value-less abstract base would validate (gts defers required-trait
    // completeness to descendants) — because these values are the chain-merge
    // *inherited defaults* (leaf wins, base fills the rest): a derived type
    // that declares only some traits inherits the generic values for the
    // holes. Dropping them would flip such sparse (customer-registered) types
    // to the trait-schema defaults (`allow_sharing: []`) — silently
    // unwritable, while still registering fine. Supplied as the typed
    // `SecretTypeTraits` carrier (via the catalog's `generic` entry) rather
    // than an untyped `json!`, so the base default is compile-checked and
    // can't drift from the catalog — the idiom from gts-rust's
    // `traits_struct_literal`.
    traits = builtin_traits("generic"),
    // The base is a pure trait/schema carrier: credentials are always typed
    // by a derived id (`generic`, `api_key`, …, or a customer type), never by
    // the bare base.
    gts_abstract = true,
    base = true
)]
pub struct CredentialV1<P: gts::GtsSchema = ()> {
    /// GTS type discriminator (the base-type contract's type field, mirroring
    /// `ModelInfoV1`). Credential types are types, not instances.
    pub gts_type: gts::GtsTypeId,
    /// Required generic payload field (the base-type contract mandates exactly
    /// one). Empty for credential types — they differ by traits, not payload.
    pub payload: P,
}

// `SecretTypeTraits` (the `x-gts-traits-schema` carrier referenced by
// `traits_schema = inline(...)` above) lives in `crate::types` next to the
// catalog descriptors it mirrors; re-exported here so the schema module keeps
// offering the trait carrier alongside the schemas built from it.
pub use crate::types::SecretTypeTraits;

// ── Credential types (derived from `CredentialV1`) ──────────────────────────
//
// One registered GTS type schema per catalog entry in
// [`crate::types::SECRET_TYPE_CATALOG`]. Registration makes each credential
// type discoverable in the types-registry and addressable as a PDP resource
// type (per-type RBAC) without new authorization machinery, and the schema's
// `x-gts-traits` mirror the catalog traits for GTS tooling.
//
// Each derived type is a `#[gts_type_schema(base = CredentialV1, …)]` unit
// struct, so the macro emits the schema and submits it to the GTS inventory
// automatically. The `x-gts-traits` value is sourced from the catalog
// descriptor (single source of truth), so a registered schema cannot drift.
// The Rust struct names below keep their shipped spelling
// (`GenericSecretV1`, …) — only the base type and the resource-type
// constant are renamed by ADR-0004 (§5.4).

#[allow(
    clippy::expect_used,
    reason = "built-in type names are compile-time constants proven present by catalog unit tests"
)]
fn builtin_traits(name: &str) -> SecretTypeTraits {
    crate::types::SecretType::from_name(name)
        .expect("built-in credential type name must be in the catalog")
        .descriptor()
        .traits()
}

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.generic.v1~"),
    description = "CredStore credential type: generic",
    properties = "",
    traits = builtin_traits("generic"),
)]
pub struct GenericSecretV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.api_key.v1~"),
    description = "CredStore credential type: api-key",
    properties = "",
    traits = builtin_traits("api-key"),
)]
pub struct ApiKeySecretV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.personal_token.v1~"),
    description = "CredStore credential type: personal-token",
    properties = "",
    traits = builtin_traits("personal-token"),
)]
pub struct PersonalTokenSecretV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.oauth2_client.v1~"),
    description = "CredStore credential type: oauth2-client",
    properties = "",
    traits = builtin_traits("oauth2-client"),
)]
pub struct Oauth2ClientSecretV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.basic_auth.v1~"),
    description = "CredStore credential type: basic-auth",
    properties = "",
    traits = builtin_traits("basic-auth"),
)]
pub struct BasicAuthSecretV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.bearer_token.v1~"),
    description = "CredStore credential type: bearer-token",
    properties = "",
    traits = builtin_traits("bearer-token"),
)]
pub struct BearerTokenSecretV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.certificate.v1~"),
    description = "CredStore credential type: certificate",
    properties = "",
    traits = builtin_traits("certificate"),
)]
pub struct CertificateSecretV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.ssh_key.v1~"),
    description = "CredStore credential type: ssh-key",
    properties = "",
    traits = builtin_traits("ssh-key"),
)]
pub struct SshKeySecretV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.webhook_hmac.v1~"),
    description = "CredStore credential type: webhook-hmac",
    properties = "",
    traits = builtin_traits("webhook-hmac"),
)]
pub struct WebhookHmacSecretV1;

#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = CredentialV1,
    type_id = gts_id!("cf.core.credstore.credential.v1~cf.core.credstore.connection_string.v1~"),
    description = "CredStore credential type: connection-string",
    properties = "",
    traits = builtin_traits("connection-string"),
)]
pub struct ConnectionStringSecretV1;

#[cfg(test)]
#[path = "gts_tests.rs"]
mod secret_type_gts_tests;
