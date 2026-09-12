//! BSS Plan & Price Modeling SDK — infrastructure-free contract crate.
//!
//! Publishes the in-process read contract (`PricingCatalogClientV1`, resolved
//! from `ClientHub`) plus the value types a consumer needs to pin a
//! `CatalogVersion` before resolving the published read model. The catalog
//! computes no monetary charge, so no arithmetic reaches this crate; the
//! publish engine, the canonical scope key and the validation pipeline are
//! gear-internal (`bss-pricing::domain`) and NOT part of the contract.
//!
//! The surface grows with the design set's slices. Today it carries the one
//! Foundation-owned consumer-facing fact: the **pin-eligibility frontier**
//! (D-136), which no consumer can evaluate for itself.

pub mod api;
pub mod catalog_version;
pub mod catalog_version_registry;
pub mod odata;
pub mod product_catalog;

pub use api::PricingCatalogClientV1;
pub use catalog_version::{CatalogVersion, PinFrontier};
pub use catalog_version_registry::{
    CatalogVersionRegistryError, CatalogVersionRegistryV1, PendingVersionRef,
    UnconfiguredCatalogVersionRegistryV1,
};
pub use product_catalog::{
    CatalogSku, CatalogTaxCategory, ProductCatalogClientV1, ProductCatalogError,
    UnconfiguredProductCatalogClientV1,
};
