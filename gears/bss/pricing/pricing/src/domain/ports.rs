//! Outbound ports this gear depends on.
//!
//! The `CatalogVersion` registry is the one external actor the publish path
//! cannot proceed without: the Product & SKU registry is the **sole**
//! incrementer, and a publish that cannot request addressability has produced
//! nothing a consumer can pin. The contract itself lives in the SDK
//! (`bss_pricing_sdk::catalog_version_registry`) so the registry gear can
//! implement it without depending on this crate; this module is where the
//! engine resolves it.
//!
//! The **product catalog** is the same registry seen from the other side, and
//! the opposite kind of dependency: nothing is refused for want of it. It is a
//! suggestion source for the surfaces that author a meter or bind a `sku_id`,
//! and its absence is reported rather than rendered as an empty catalog.

pub mod metrics;

use toolkit_canonical_errors::CanonicalError;

pub use bss_pricing_sdk::catalog_version_registry::{
    CATALOG_VERSION_REJECTED, CatalogVersionRegistryError, CatalogVersionRegistryV1,
    PendingVersionRef, UnconfiguredCatalogVersionRegistryV1, registry_rejected,
    registry_unreachable, unconfigured_registry,
};
pub use bss_pricing_sdk::product_catalog::{
    CatalogSku, CatalogTaxCategory, ProductCatalogClientV1, ProductCatalogError,
    UnconfiguredProductCatalogClientV1, unconfigured_catalog,
};

use crate::domain::error::DomainError;

/// Map a registry failure onto the gear's rejection vocabulary.
///
/// Every registry failure lands on the same fail-closed answer **in this gear**:
/// the publish does not become addressable, and inventing a local version would
/// make this gear a second incrementer.
///
/// # Why "rejected" is nonetheless its own variant
///
/// This function used to collapse all four errors onto
/// [`DomainError::CatalogVersionUnavailable`], and argued the fold with *"the
/// distinction between 'unreachable' and 'rejected' is diagnostic, not
/// behavioural — neither produces a version"*. That is true of **this gear's**
/// behaviour and false of the **caller's**: `CatalogVersionUnavailable` renders a
/// 503, a 503 is a retry instruction, and retry policy is exactly the behaviour a
/// status code selects. `Unconfigured`, `Unreachable` and `Internal` are
/// deployment states a later request may find changed, so retry is the right
/// instruction. `Rejected` is a *decision* — an unknown SKU, a closed version —
/// and it will be made identically for as long as the request is made unchanged,
/// so a client honouring the 503 retried forever against an answer that never
/// moved. It maps to [`DomainError::CatalogVersionRejected`] and a 400 carrying
/// `CATALOG_VERSION_REJECTED`, which is the discriminator the bare 503 had none
/// of. (`infra::error_mapping` carries the wire half.)
///
/// Deliberately a named function rather than a `From` impl. `DomainError` is a
/// `Clone + Eq` value type carried into responses and compared in tests, so it
/// cannot hold a boxed source and the chain flattens here whatever the shape;
/// making the conversion explicit at least keeps it visible at the call site
/// instead of happening silently through a `?`.
#[must_use]
pub fn registry_failure(err: CanonicalError) -> DomainError {
    let err = CatalogVersionRegistryError::from(err);
    match &err {
        // The one arm the registry answered. Matched on the variant rather than
        // on its rendered string, so a reworded `#[error(...)]` in the SDK cannot
        // silently move a refusal back onto the retriable arm.
        CatalogVersionRegistryError::Rejected(_) => {
            DomainError::CatalogVersionRejected(err.to_string())
        }
        CatalogVersionRegistryError::Unconfigured
        | CatalogVersionRegistryError::Unreachable(_)
        | CatalogVersionRegistryError::Internal(_)
        | CatalogVersionRegistryError::Other(_) => {
            DomainError::CatalogVersionUnavailable(err.to_string())
        }
    }
}
