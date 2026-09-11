#![doc = include_str!("../README.md")]
pub mod api;
pub mod client;
pub mod config;
pub mod domain;
pub mod gear;
pub(crate) mod gts;
pub mod infra;

pub use gear::CredStoreGear;
