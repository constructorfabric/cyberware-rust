// Link the gears whose `#[toolkit::gear]` inventory registrations must be
// present in this OoP binary.
//
// This binary links ONLY:
//   * the target gear (this crate's own library), and
//   * the tenant-plane authn stack it needs to turn a bearer token into a
//     `SecurityContext` locally (authn-resolver + static-authn-plugin +
//     types-registry).
//
// It deliberately does NOT link api-gateway, grpc-hub, tenant-resolver, or
// authz-resolver: the HTTP surface comes from the OoP bootstrap, the directory
// is dialed remotely (`TOOLKIT_DIRECTORY_ENDPOINT`), and authorization is
// resolved over REST from the platform-host via `#[toolkit::consumes]`.
#![allow(unused_imports)]

// Target gear (this crate's library target).
use users_info as _;

// Tenant-plane authn stack (embedded per pod).
use authn_resolver as _;
use static_authn_plugin as _;
use types_registry as _;
