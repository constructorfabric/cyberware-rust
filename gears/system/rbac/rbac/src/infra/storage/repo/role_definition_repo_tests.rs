//! Pure-function tests for the SeaORM-backed repo's error-mapping
//! seams. The constraint-hint mechanics themselves (PG fast path,
//! `SQLite` column-set fallback, quoted-value extraction) live in
//! [`crate::infra::canonical_mapping`] and are covered by its
//! sibling `canonical_mapping_tests.rs`; this file only pins the
//! repo-level refinement behaviour and the `ScopeError` wrapper.

#![allow(clippy::panic)]

use super::*;

#[test]
fn map_db_err_falls_through_to_internal_for_non_sql_dberr() {
    // `DbErr::Custom` carries no SqlErr discriminator: no constraint
    // hint is extracted, and the central classifier routes it to
    // `Internal` with a redacted diagnostic.
    let err = DbErr::Custom("synthetic".to_owned());
    let mapped = map_db_err("create", err);
    match mapped {
        DomainError::Internal { diagnostic, .. } => {
            assert!(
                diagnostic.contains("unclassified database error"),
                "internal diagnostic should mention the classifier path, got: {diagnostic}"
            );
        }
        other => panic!("expected Internal, got {other:?}"),
    }
}

// -----------------------------------------------------------------
// map_scope_err — the non-Db variants surface as Internal
// -----------------------------------------------------------------

#[test]
fn map_scope_err_invalid_variant_surfaces_as_internal() {
    let err = ScopeError::Invalid("test scope misconfiguration");
    let mapped = map_scope_err("update", err);
    match mapped {
        DomainError::Internal { diagnostic, .. } => {
            assert!(diagnostic.contains("scope error"));
            assert!(diagnostic.contains("update"));
        }
        other => panic!("expected Internal, got {other:?}"),
    }
}

#[test]
fn map_scope_err_denied_variant_surfaces_as_internal() {
    let err = ScopeError::Denied("synthetic denial");
    let mapped = map_scope_err("delete", err);
    assert!(matches!(mapped, DomainError::Internal { .. }));
}

#[test]
fn map_scope_err_db_variant_delegates_to_map_db_err() {
    let err = ScopeError::Db(DbErr::Custom("synthetic db".to_owned()));
    let mapped = map_scope_err("create", err);
    assert!(matches!(mapped, DomainError::Internal { .. }));
}
