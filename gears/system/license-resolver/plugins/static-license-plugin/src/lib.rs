//! Static License Resolver Plugin
//!
//! Reference backend for the license resolver: grant facts come from
//! configuration rather than a licensing service, so a deployment can exercise
//! the whole check path without one.
//!
//! Deny by default — a check is granted only if some configured rule matches it
//! in full. An empty rule list therefore grants nothing.
//!
//! ## Configuration
//!
//! ```yaml
//! gears:
//!   static-license-plugin:
//!     config:
//!       vendor: "constructorfabric"
//!       priority: 100
//!       grants:
//!         - resource_type: "gts.cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~"
//!           resource_id: "gpt-4o"          # omit to license the whole type
//!           subject_type: "gts.cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~"
//!           resource_metadata:             # optional attribute constraints
//!             model_vendor: "openai"
//! ```
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod config;
pub mod domain;
pub mod gear;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod test_support;

pub use gear::StaticLicensePlugin;
