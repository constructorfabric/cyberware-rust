//! REST transport layer (`DESIGN.md` §1.3 Architecture Layers, §3.3 API
//! Contracts).
//!
//! `dto` and `extractors` (mentioned in earlier scaffolding) don't exist as
//! separate modules - DTOs are colocated in each `handlers/*.rs` file
//! (design.md "DTOs are colocated in each handler file") and no custom
//! extractors are needed yet.

pub mod error;
pub mod handlers;
pub mod pagination;
pub mod routes;
pub mod state;
