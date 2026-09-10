// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use super::*;

#[test]
fn unique_violation_on_dedup_is_dedup_conflict() {
    assert_eq!(
        classify_db("23505", Some("usage_records_dedup_uniq")),
        DbErrorClass::DedupUniqueViolation
    );
}
#[test]
fn unique_violation_on_catalog_pk_is_type_exists() {
    assert_eq!(
        classify_db("23505", Some("usage_type_catalog_pkey")),
        DbErrorClass::CatalogUniqueViolation
    );
}
#[test]
fn unique_violation_on_unknown_constraint_is_other() {
    // A future second unique constraint (or a records PK collision) must not be
    // misclassified as a catalog-specific violation.
    assert_eq!(
        classify_db("23505", Some("usage_records_pkey")),
        DbErrorClass::Other
    );
}
#[test]
fn unique_violation_without_constraint_is_other() {
    assert_eq!(classify_db("23505", None), DbErrorClass::Other);
}
#[test]
fn fk_violation_is_type_referenced() {
    assert_eq!(
        classify_db("23503", Some("usage_records_gts_id_fk")),
        DbErrorClass::ForeignKeyViolation
    );
}

/// `PostgreSQL` 18 reports `ON DELETE RESTRICT` as the standard 23001
/// `restrict_violation`, where <= 17 reported 23503. Both are "the row is
/// still referenced" for this plugin; dropping either one would turn a
/// `UsageTypeReferenced` back into an opaque `Internal`.
#[test]
fn restrict_violation_is_also_type_referenced() {
    assert_eq!(
        classify_db("23001", Some("usage_records_gts_id_fk")),
        DbErrorClass::ForeignKeyViolation
    );
}
#[test]
fn restrict_violation_is_type_referenced() {
    // PostgreSQL 18 reports an `ON DELETE RESTRICT` refusal as `23001` rather
    // than `23503`. Both must reach `ForeignKeyViolation`, or `delete` answers
    // `Internal` instead of `UsageTypeReferenced` on one PostgreSQL major --
    // which is exactly how this surfaced, as a version-dependent test failure.
    assert_eq!(
        classify_db("23001", Some("usage_records_gts_id_fk")),
        DbErrorClass::ForeignKeyViolation
    );
}
#[test]
fn connection_class_is_transient() {
    assert_eq!(classify_db("08006", None), DbErrorClass::Transient);
    assert_eq!(classify_db("57P03", None), DbErrorClass::Transient);
}
#[test]
fn unknown_code_is_other() {
    assert_eq!(classify_db("42601", None), DbErrorClass::Other);
}

#[test]
fn pool_timed_out_while_saturated_does_not_clear_readiness() {
    // A saturated-but-healthy pool returns PoolTimedOut while still holding its
    // established connections. Clearing readiness here flaps the gauge and
    // raises a false `ready == 0` outage signal under load.
    assert!(!acquire_error_clears_readiness(
        &sqlx::Error::PoolTimedOut,
        8 // live connections (pool at capacity)
    ));
}

#[test]
fn pool_timed_out_with_no_live_connections_clears_readiness() {
    // The connection-refused / unreachable-backend case: sqlx surfaces it as a
    // PoolTimedOut after the acquire timeout, but the pool holds zero live
    // connections. This is a genuine outage and must clear readiness.
    assert!(acquire_error_clears_readiness(
        &sqlx::Error::PoolTimedOut,
        0
    ));
}

#[test]
fn connectivity_failures_clear_readiness() {
    use std::io;
    assert!(
        acquire_error_clears_readiness(
            &sqlx::Error::Io(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "connection refused"
            )),
            4
        ),
        "a refused/lost physical connection is a genuine connectivity outage"
    );
    assert!(
        acquire_error_clears_readiness(&sqlx::Error::PoolClosed, 4),
        "a closed pool means the backend is no longer serving connections"
    );
}

#[test]
fn non_connectivity_errors_do_not_clear_readiness() {
    // A query-shaped error reaching the acquire predicate (defensively) is not
    // an outage signal, regardless of pool occupancy.
    assert!(!acquire_error_clears_readiness(
        &sqlx::Error::RowNotFound,
        0
    ));
}

#[test]
fn tls_error_at_query_time_maps_to_transient() {
    // A TLS transport blip at query time is a connectivity-class failure:
    // `acquire_error_clears_readiness` already treats `Tls` as an outage (it
    // clears the `ready` gauge). The catch-all query-error mapping must agree
    // and classify it retryable (Transient), not a non-retryable Internal —
    // otherwise readiness and retry-ability disagree on the same fault.
    let err = sqlx::Error::Tls(Box::new(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "tls handshake failed",
    )));
    match map_sqlx_err(&err) {
        UsageCollectorPluginError::Transient { detail, .. } => {
            // Still DSN-free: a fixed token, never the raw sqlx Display.
            assert_eq!(detail, "database unavailable");
        }
        other => panic!("expected Transient for a TLS transport error, got {other:?}"),
    }
}

#[test]
fn internal_mapping_does_not_leak_raw_error_text() {
    // The SDK contract (UsageCollectorPluginError::Internal) requires the detail
    // to be DSN-free / pre-redacted. A `sqlx::Error` Display (Configuration/Tls
    // source chains) can carry connection-string fragments, so the catch-all
    // must not format the raw error into the user-facing detail.
    let mapped = map_sqlx_err(&sqlx::Error::RowNotFound);
    match mapped {
        UsageCollectorPluginError::Internal(detail) => {
            assert_eq!(
                detail, "database error",
                "Internal detail must be a fixed, DSN-free string"
            );
            assert!(
                !detail.contains("no rows"),
                "the raw sqlx::Error Display must not leak into the detail"
            );
        }
        other => panic!("expected Internal, got {other:?}"),
    }
}
