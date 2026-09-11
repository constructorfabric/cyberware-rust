//! Gear Orchestrator Contracts
//!
//! Domain contracts and client interfaces for gear orchestration.
//! This crate provides the `DirectoryClient` trait and related types that
//! define the contract for service discovery and instance management.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

mod api;
#[cfg(feature = "grpc")]
mod grpc;
pub mod labels;

pub use api::{
    DirectoryClient, DirectoryInvalidArgument, DirectoryNotFound, DirectoryPermissionDenied,
    DirectoryServiceNameConflict, GrpcServiceInfo, InstanceState, LabelSelector,
    RegisterInstanceInfo, ServiceEndpoint, ServiceInstanceInfo,
};
#[cfg(feature = "grpc")]
pub use grpc::*;
pub use labels::{
    LabelValidationError, MAX_LABEL_KEY_LEN, MAX_LABEL_VALUE_LEN, MAX_LABELS, validate_labels,
    validate_selector,
};
