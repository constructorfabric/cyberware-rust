//! Unit tests for the pure resolution helpers: provider-precedence ordering and
//! the per-pair staleness window. These exercise the money-relevant decisions
//! ([`order_index`] / [`is_stale`] / [`is_g10_pair`]) without a database; the
//! DB-bound `resolve` end-to-end path is left to a testcontainer test the
//! controller adds.

use super::*;

fn cfg(g10_hours: u64, default_days: u64, order: &[&str]) -> FxConfig {
    FxConfig {
        revaluation_enabled: false,
        stale_g10_hours: g10_hours,
        stale_default_max_days: default_days,
        rate_sync_tick_secs: 3_600,
        revaluation_run_tick_secs: 86_400,
        provider_order: order.iter().map(|s| (*s).to_owned()).collect(),
        provider_vendor: "cf.bss".to_owned(),
    }
}

// --- staleness window ---

#[test]
fn g10_pair_fresh_just_under_24h() {
    // A G10 pair (USD/EUR) at 23h age is within the 24h window → fresh.
    let cfg = cfg(24, 7, &[]);
    assert!(!is_stale("USD", "EUR", time::Duration::hours(23), &cfg));
}

#[test]
fn g10_pair_stale_past_24h() {
    // The same pair at 25h age has crossed the 24h window → stale.
    let cfg = cfg(24, 7, &[]);
    assert!(is_stale("USD", "EUR", time::Duration::hours(25), &cfg));
}

#[test]
fn g10_window_boundary_is_exclusive_at_exactly_24h() {
    // Exactly at the window (age == 24h) is NOT stale (`age > window` is strict);
    // one second past it is.
    let cfg = cfg(24, 7, &[]);
    assert!(!is_stale("USD", "EUR", time::Duration::hours(24), &cfg));
    assert!(is_stale(
        "USD",
        "EUR",
        time::Duration::hours(24) + time::Duration::seconds(1),
        &cfg
    ));
}

#[test]
fn g10_classification_keys_on_either_leg() {
    // Either side being G10 makes the pair G10 (here the quote leg, USD).
    assert!(is_g10_pair("BRL", "USD"));
    assert!(is_g10_pair("USD", "BRL"));
    // Neither leg G10 → not a G10 pair.
    assert!(!is_g10_pair("BRL", "INR"));
}

#[test]
fn non_g10_pair_uses_max_days_window() {
    // A non-G10 pair (BRL/INR) tolerates up to the configured max-days window:
    // fresh at 6 days, stale past 7.
    let cfg = cfg(24, 7, &[]);
    assert!(!is_stale("BRL", "INR", time::Duration::days(6), &cfg));
    assert!(is_stale("BRL", "INR", time::Duration::days(8), &cfg));
}

#[test]
fn non_g10_uses_days_even_when_g10_hour_window_would_pass() {
    // Guard the branch: a non-G10 pair at 30h is NOT stale (it is on the day-scale
    // window), even though 30h exceeds the 24h G10 window. Catches a base/quote
    // mix-up that mis-routes a non-G10 pair through the tighter window.
    let cfg = cfg(24, 7, &[]);
    assert!(!is_stale("BRL", "INR", time::Duration::hours(30), &cfg));
}

#[test]
fn future_as_of_is_never_stale() {
    // A negative age (future as_of: clock skew) is within any window.
    let cfg = cfg(24, 7, &[]);
    assert!(!is_stale("USD", "EUR", time::Duration::hours(-5), &cfg));
    assert!(!is_stale("BRL", "INR", time::Duration::hours(-5), &cfg));
}

// --- provider precedence ---

#[test]
fn order_index_ranks_by_configured_position() {
    let order = ["ecb".to_owned(), "boe".to_owned(), "fed".to_owned()];
    assert_eq!(order_index("ecb", &order), 0);
    assert_eq!(order_index("boe", &order), 1);
    assert_eq!(order_index("fed", &order), 2);
}

#[test]
fn unlisted_provider_sorts_after_every_listed_one() {
    let order = ["ecb".to_owned(), "boe".to_owned()];
    // An unlisted provider gets rank == len, strictly greater than every listed
    // rank — so it always sorts last.
    assert_eq!(order_index("acme", &order), 2);
    assert!(order_index("acme", &order) > order_index("boe", &order));
}

#[test]
fn empty_order_makes_every_provider_equal_rank() {
    // With no configured order, all providers share rank 0; the resolver's stable
    // secondary key (fallback_order, then provider) then decides.
    let order: [String; 0] = [];
    assert_eq!(order_index("ecb", &order), 0);
    assert_eq!(order_index("acme", &order), 0);
}
