//! Infrastructure → domain error mapping.
//!
//! `ChatEngineError` (in `domain/error.rs`) is framework-agnostic and must not
//! import database types (enforced by DE0301). The conversions from SeaORM /
//! `toolkit_db` error types therefore live here, in the infrastructure layer,
//! where depending on `sea_orm` / `toolkit_db` is legitimate. Because these
//! `From` impls are in the same crate as `ChatEngineError`, the `?` operator in
//! repositories still converts DB errors into the domain error transparently.
//
// @cpt-cf-chat-engine-domain-error:p2

use sea_orm::DbErr;
use toolkit_db::DbError;
use toolkit_db::secure::ScopeError;

use crate::domain::error::ChatEngineError;

impl From<DbErr> for ChatEngineError {
    fn from(err: DbErr) -> Self {
        match err {
            DbErr::RecordNotFound(msg) => Self::NotFound {
                resource: "record",
                id: msg,
            },
            other => {
                // The typed error is preserved in `source` (reachable via
                // `.source()` / downcast); the reason is a category label so
                // we don't flatten the chain into the message (DE1302).
                Self::Internal {
                    reason: "database error".to_owned(),
                    source: Some(Box::new(other)),
                }
            }
        }
    }
}

impl From<DbError> for ChatEngineError {
    fn from(err: DbError) -> Self {
        match err {
            // Route SeaORM errors through the existing classifier so
            // `RecordNotFound` keeps its 404 mapping.
            DbError::Sea(sea) => sea.into(),
            other => Self::Internal {
                // Source preserves the typed error / chain (DE1302).
                reason: "database error".to_owned(),
                source: Some(Box::new(other)),
            },
        }
    }
}

impl From<ScopeError> for ChatEngineError {
    fn from(err: ScopeError) -> Self {
        match err {
            ScopeError::Db(sea) => sea.into(),
            ScopeError::Denied(msg) => Self::Forbidden {
                reason: msg.to_owned(),
            },
            ScopeError::TenantNotInScope { tenant_id } => Self::Forbidden {
                reason: format!("tenant {tenant_id} not in scope"),
            },
            // `Invalid` flags a programmer error in scoping config; it
            // would surface to the operator, not the caller.
            ScopeError::Invalid(msg) => Self::Internal {
                reason: format!("invalid scope: {msg}"),
                source: None,
            },
            // `ScopeError` is `#[non_exhaustive]`: variants this gear has no
            // specific answer for (today the graph-query refusals, which it can
            // never trigger) map to an internal error, like `Invalid`.
            other => Self::Internal {
                reason: format!("invalid scope: {other}"),
                source: None,
            },
        }
    }
}

#[cfg(test)]
#[path = "error_map_tests.rs"]
mod error_map_tests;
