//! Sub-second TTLs against a `leaseDurationSeconds` `int32` field (DESIGN.md §2.10).
//!
//! `Lease.spec.leaseDurationSeconds` is an `int32` in **seconds**, but cluster
//! lock/election TTLs are `Duration`s and the rate-limiting pattern holds locks for
//! sub-second windows. So the interop-facing field is a rounded-**up** over-estimate
//! ([`lease_duration_seconds`]) — nothing that reads only the standard field ever
//! thinks a still-held lock is free — while the exact requested TTL rides the
//! `ttl-ms` annotation ([`ttl_ms`]), which the §2.9 expiry test actually uses. The
//! asymmetry is one-directional: a foreign reader is too conservative, never too
//! aggressive, which is the only safe direction to disagree.

use std::time::Duration;

use cluster_sdk::ClusterError;

/// The `leaseDurationSeconds` (`int32`) to write for a `ttl`: `max(1, ceil(ttl))`.
///
/// Always a safe over-estimate — a 750 ms TTL becomes `1`, never `0`, so a foreign
/// reader cannot conclude the lock is free while this plugin still holds it.
///
/// # Errors
///
/// Returns [`ClusterError::InvalidConfig`] when `ttl` rounds up beyond `i32::MAX`
/// seconds (~68 years): a `Duration` can express it but the `int32` field cannot.
pub fn lease_duration_seconds(ttl: Duration) -> Result<i32, ClusterError> {
    // ceil to whole seconds without floating point: (ms + 999) / 1000, floored at 1.
    let millis = ttl.as_millis();
    let secs_ceil = millis.div_ceil(1000).max(1);
    i32::try_from(secs_ceil).map_err(|_| ClusterError::InvalidConfig {
        reason: format!(
            "ttl {ttl:?} exceeds the Lease `leaseDurationSeconds` int32 ceiling (~68 years)"
        ),
    })
}

/// The exact requested TTL in whole milliseconds, for the `ttl-ms` annotation.
///
/// # Errors
///
/// Returns [`ClusterError::InvalidConfig`] on the same over-`i32::MAX`-seconds TTL
/// [`lease_duration_seconds`] rejects, so the two encodings can never disagree about
/// acceptability.
pub fn ttl_ms(ttl: Duration) -> Result<u64, ClusterError> {
    // Reuse the seconds ceiling as the acceptability gate, so a TTL is either
    // representable in both encodings or rejected by both.
    lease_duration_seconds(ttl)?;
    // After that gate, milliseconds fit u64 comfortably (< i32::MAX * 1000).
    Ok(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX))
}

/// Rejects an election TTL below `min_election_ttl`, naming the write rate it would
/// otherwise generate (DESIGN.md §2.10). Locks have no such floor — a lock's writes
/// are per-acquisition, not per-interval.
///
/// `max_missed` is the election's missed-renewal budget; the derived renewal
/// interval is `ttl / (max_missed + 1)`, and the rate the message reports is its
/// reciprocal.
///
/// # Errors
///
/// Returns [`ClusterError::InvalidConfig`] when `ttl < min_election_ttl`.
pub fn check_election_ttl_floor(
    ttl: Duration,
    min_election_ttl: Duration,
    max_missed: u32,
) -> Result<(), ClusterError> {
    if ttl >= min_election_ttl {
        return Ok(());
    }
    let renewal = ttl / (max_missed + 1);
    let rate = if renewal.is_zero() {
        f64::INFINITY
    } else {
        1.0 / renewal.as_secs_f64()
    };
    Err(ClusterError::InvalidConfig {
        reason: format!(
            "election ttl {ttl:?} is below min_election_ttl {min_election_ttl:?}: its derived \
             renewal rate is ~{rate:.1} writes/sec against the API server"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::{check_election_ttl_floor, lease_duration_seconds, ttl_ms};
    use std::time::Duration;

    #[test]
    fn ceil_table() {
        let cases = [
            (Duration::from_millis(1), 1),
            (Duration::from_millis(750), 1),
            (Duration::from_secs(1), 1),
            (Duration::from_millis(1001), 2),
            (Duration::from_secs(29), 29),
            (Duration::from_millis(29_001), 30),
        ];
        for (ttl, expected) in cases {
            assert_eq!(lease_duration_seconds(ttl).unwrap(), expected, "{ttl:?}");
        }
    }

    #[test]
    fn ttl_ms_is_exact() {
        assert_eq!(ttl_ms(Duration::from_millis(750)).unwrap(), 750);
        assert_eq!(ttl_ms(Duration::from_millis(1)).unwrap(), 1);
        assert_eq!(ttl_ms(Duration::from_secs(30)).unwrap(), 30_000);
    }

    #[test]
    fn over_i32_max_seconds_is_invalid_config() {
        let huge = Duration::from_secs(u64::from(u32::MAX)); // > i32::MAX seconds
        assert!(matches!(
            lease_duration_seconds(huge),
            Err(cluster_sdk::ClusterError::InvalidConfig { .. })
        ));
        assert!(ttl_ms(huge).is_err());
    }

    #[test]
    fn election_floor_rejects_and_names_the_rate() {
        // 1 s TTL, budget 2 → 333 ms renewal → ~3 writes/sec.
        let err = check_election_ttl_floor(Duration::from_secs(1), Duration::from_secs(5), 2)
            .unwrap_err();
        let cluster_sdk::ClusterError::InvalidConfig { reason } = err else {
            panic!("expected InvalidConfig");
        };
        assert!(reason.contains("writes/sec"), "{reason}");
        // At or above the floor is fine.
        assert!(
            check_election_ttl_floor(Duration::from_secs(5), Duration::from_secs(5), 2).is_ok()
        );
        assert!(
            check_election_ttl_floor(Duration::from_secs(30), Duration::from_secs(5), 2).is_ok()
        );
    }
}
