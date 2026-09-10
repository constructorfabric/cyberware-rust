//! `CatalogVersion` and the pin-eligibility frontier.
//!
//! A consumer resolves the published read model at exactly one **pin-eligible**
//! `CatalogVersion` for the duration of a resolution or rating run. Whether a
//! version is pin-eligible is a version-level, prefix-closed predicate
//! (`design/01-foundation.md` §4.4, D-101 + D-114): the version is committed,
//! every subject row it projects is warm-complete, and every earlier version is
//! itself pin-eligible. No consumer can evaluate that for itself, so the gear
//! materializes the frontier and publishes it (D-136) — this type is what it
//! serves.
//!
//! **What a pin buys today, stated because the predicate above does not say
//! it.** The frontier is real: a version reaches it only once every subject the
//! gear knows it published is warm. What a consumer cannot yet do is *resolve*
//! anything at that pin **through this SDK** — `PricingCatalogClientV1` has no
//! implementor, which is what D-347 records as owed.
//!
//! Two surfaces do resolve at the pin frontier: `GET /plans/{planId}/preview`
//! and `GET /plans/{planId}/sellability` read the frontier and then the delta at
//! that version, and both are mounted, authorized and route-tested. Saying the
//! gear has no resolution surface at all is what sends a reader chasing a route
//! that exists. What no surface does is resolve the **full published payload**
//! the PRD asks for, and the delta a version freezes deliberately omits the facts
//! whose slices are unbuilt (the `PriceWindow` intervals and the derived
//! coverage end, the GA-gate flags, the registry `sellable` flag, and the grant
//! set). So a version is pinnable and its payload is incomplete, and a reader is
//! entitled to know which of the six sellability predicates it can evaluate from
//! one: not (1), not (5), not (6).
//!
//! The Slice-6 cross-boundary contract is **not** among those omissions: D-169
//! struck `crossBoundaryWarningText` from the contract — the copy belongs to the
//! surface that renders it, PRD AC #66 — so the marker waits on nothing, and
//! every resolved plan subject carries it.

use time::OffsetDateTime;
/// A committed catalog version. Monotonic per tenant; the registry (Product &
/// SKU) is the sole incrementer, so this crate only ever carries a value the
/// catalog received, never one it derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CatalogVersion(u64);

impl CatalogVersion {
    /// Wrap a committed version number.
    #[must_use]
    pub const fn new(version: u64) -> Self {
        Self(version)
    }

    /// The underlying version number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The tenant's current pin-eligibility frontier: the newest version a consumer
/// may pin, and when it last advanced.
///
/// `advanced_at` is the referent of the ≤ 5s pin-lag rule and of the
/// `pricing.readmodel.pin_eligibility_overdue` alarm — a stuck version holds the
/// frontier, which is exactly what the alarm signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinFrontier {
    /// The newest pin-eligible version. Advanced only forward, and only by the
    /// projector inside the transaction completing the frontier's next version
    /// in order — a later version's completion never advances it past a gap.
    pub catalog_version: CatalogVersion,
    /// UTC instant the frontier last advanced.
    pub advanced_at: OffsetDateTime,
}
