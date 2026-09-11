//! Gear Orchestrator
//!
//! System gear for service discovery.
//! This gear provides `DirectoryService` for gRPC service registration and discovery.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

// === MODULE DEFINITION ===
pub mod gear;
pub use gear::GearOrchestrator;

// === INTERNAL MODULES (pub for integration tests) ===
pub mod api;
mod config;
pub mod domain;
mod server;

// === RE-EXPORTS ===
pub use cf_system_sdks::directory::{
    DirectoryGrpcClient, RegisterInstanceInfo, ServiceEndpoint, ServiceInstanceInfo,
};
