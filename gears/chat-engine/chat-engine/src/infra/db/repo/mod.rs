//! Sea-ORM-backed repository implementations.
//!
//! Each module here exposes a `*Repo` trait and a Sea-ORM impl. Services
//! depend on the trait (object-safe `Arc<dyn …>`) so unit tests can swap in
//! in-memory mocks without touching a database.
//
// @cpt-cf-chat-engine-infra-repo-root:p3

pub mod message_repo;
pub mod plugin_config_repo;
pub mod reaction_repo;
pub mod session_repo;
pub mod session_type_repo;
pub mod stream_event_repo;
pub mod variant_repo;

/// Crate-wide `DBProvider` alias parameterised over the chat-engine
/// domain error.
///
/// Modules elsewhere in the workspace alias `DBProvider<DomainError>` the
/// same way — the alias lets repos receive a single `Arc<ChatEngineDb>`
/// handle whose `conn()` and `transaction_with_config(...)` results map
/// cleanly into [`crate::domain::error::ChatEngineError`] via the
/// `From<DbError>` impl on the error enum.
pub type ChatEngineDb = toolkit_db::DBProvider<crate::domain::error::ChatEngineError>;

/// Parse a caller-supplied owner id into a `Uuid`.
///
/// The session owner columns (`sessions.tenant_id` / `sessions.user_id`) and
/// the denormalized child columns (`owner_tenant_id` / `owner_id`) are UUID
/// columns as of `m20260417_000006_authz_owner_columns`, while the legacy
/// owner-filtered repo methods still take the ids as `&str` (they come from
/// `Identity`, which carries them as opaque strings). Comparing a UUID column
/// against a string predicate does NOT work: Postgres rejects `uuid = text`
/// outright, and on SQLite the value is stored as a BLOB by the driver, so a
/// text comparison silently matches nothing — a legitimately owned row then
/// reads back as `NotFound`. Every owner predicate therefore parses first and
/// compares Uuid-to-Uuid.
///
/// # Errors
///
/// [`ChatEngineError::Internal`] when `value` is not a UUID. This is an
/// invariant violation rather than user input: the ids originate in the
/// verified `SecurityContext`.
pub(crate) fn parse_owner_uuid(
    value: &str,
    field: &str,
) -> Result<uuid::Uuid, crate::domain::error::ChatEngineError> {
    value.parse::<uuid::Uuid>().map_err(|err| {
        crate::domain::error::ChatEngineError::internal(format!(
            "session {field} is not a valid UUID: {err}"
        ))
    })
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod mod_tests;
