// Ensure all platform-host gears are linked and registered via inventory.
//
// Unlike `cf-gears-example-server`, this crate links ONLY the trust-coupled
// core + system gears that make up the platform-host image (see DESIGN
// "Platform Host Composition"). Gear isolation for OoP images is achieved by
// the dependency graph, not `#[cfg]` gates.
#![allow(unused_imports)]

// Trust-coupled core
use account_management as _;
use authz_resolver as _;
use resource_group as _;
use tenant_resolver as _;

// System gears
use api_gateway as _;
use authn_resolver as _;
use credstore as _;
use gear_orchestrator as _;
use grpc_hub as _;
use types_registry as _;

// === Plugins (selected via Cargo features; active vendor chosen by config) ===

#[cfg(feature = "static-authn")]
use static_authn_plugin as _;

#[cfg(feature = "oidc-authn")]
use oidc_authn_plugin as _;

#[cfg(feature = "static-authz")]
use static_authz_plugin as _;

#[cfg(feature = "tr-authz")]
use tr_authz_plugin as _;

#[cfg(feature = "static-tenants")]
use static_tr_plugin as _;

#[cfg(feature = "single-tenant")]
use single_tenant_tr_plugin as _;

#[cfg(feature = "tenant-resolver-rg")]
use rg_tr_plugin as _;

#[cfg(feature = "static-credstore")]
use static_credstore_plugin as _;

#[cfg(feature = "static-idp")]
use static_idp_plugin as _;
