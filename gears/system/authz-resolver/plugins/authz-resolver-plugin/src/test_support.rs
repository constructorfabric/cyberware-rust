//! Test-support fakes for the plugin's own tests. Gated behind the
//! `test-support` Cargo feature so production builds do not link them.
//!
//! Every fake here is intentionally **minimal**. Methods no current test
//! exercises are stubbed with `unreachable!` carrying a message, so the first
//! test to reach one gets an immediate, specific signal to extend the fake
//! rather than a silently wrong default.

mod rbac_fake;
mod registry_fake;
pub mod request_builder;
mod resource_group_fake;
mod tenant_resolver_fake;
pub mod trusted_actors;

pub use crate::domain::clock::StubClock;
pub use rbac_fake::{InMemoryRbacServiceClient, Script as RbacScript};
pub use registry_fake::RecordingTypesRegistry;
pub use request_builder::EvaluationRequestBuilder;
pub use resource_group_fake::{InMemoryResourceGroupClient, StuckCursor, StuckTarget};
pub use tenant_resolver_fake::InMemoryTenantResolverClient;
