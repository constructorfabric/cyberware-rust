//! `toolkit_db` error conversions this backend needs.

/// `toolkit_db::secure::ScopeError` -> `toolkit_db::DbError`, so a transaction
/// closure (fixed to `DBProvider<DbError>`'s error type, per
/// `toolkit_db::DBProvider::transaction`'s signature) can propagate a scope
/// violation through `?` like any other DB failure. A scope violation inside
/// this backend's own transactions would indicate a bug in its own
/// `AccessScope` usage, not a caller mistake, so folding it into the same
/// generic-failure path the closure already uses is appropriate.
pub fn db_err_from_scope(err: &toolkit_db::secure::ScopeError) -> toolkit_db::DbError {
    toolkit_db::DbError::Other(anyhow::anyhow!("{err}"))
}
