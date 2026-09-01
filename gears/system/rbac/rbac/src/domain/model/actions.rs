//! Lowercase `snake_case` action constants shared by REST handlers,
//! `gts/permissions.rs` declarations, and `PolicyEnforcer` calls.
//!
//! Backed by [`rbac_sdk::models::Action`] so the wire-form strings live
//! in exactly one place. The aliases stay as `&'static str` constants for
//! ergonomics at call sites that compare to `PermissionRule.operation`.

use rbac_sdk::models::Action;

pub const READ: &str = Action::Read.as_str();
pub const WRITE: &str = Action::Write.as_str();
pub const DELETE: &str = Action::Delete.as_str();
