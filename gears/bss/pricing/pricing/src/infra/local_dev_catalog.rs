//! A **static, invented** product catalog, for deployments with no registry gear
//! to ask.
//!
//! # This is fabricated catalog content, on purpose
//!
//! [`ProductCatalogClientV1`](crate::domain::ports::ProductCatalogClientV1) is
//! the registry's read contract, and the registry is not in this repository:
//! `gears/bss/products` holds a PRD and no code. Without *some* source the SKU
//! pick-lists can only offer what the tenant has already priced, so the first
//! row for a new SKU is typed from memory and the plan's `sku_id` is typed as a
//! bare UUID — which is the state the stand was in.
//!
//! So the trade is stated rather than hidden, and it is a **worse trade than
//! [`local_dev_registry`](crate::infra::local_dev_registry)'s** in one specific
//! way worth being plain about. An invented `CatalogVersion` is a number that
//! collides with a real registry's numbering; an invented SKU is *content an
//! operator cannot tell from the real catalog*, and a row priced against one
//! carries its unit into a real scope key that outlives the demo.
//!
//! Four properties keep that visible rather than invisible:
//!
//! * **It is opt-in by a value that says what it is.** The config takes
//!   `mode = "local_dev_static_skus"`, which cannot be set by accident.
//! * **Every id is from one reserved namespace.** Each `sku_id` begins
//!   [`DEV_LOCAL_SKU_PREFIX`], so a plan bound to a fabricated SKU is found later
//!   with `WHERE sku_id::text LIKE 'ddddddd%'` — the same sweepability the
//!   invented version refs have.
//! * **Every code is prefixed `DEV-`.** The operator reading the pick-list sees
//!   it in the list, not only in a config file they will never open.
//! * **It says so on every boot**, at `warn!`, naming the mode.
//!
//! # Why these SKUs
//!
//! They are the Pricing Studio prototype's own set, trimmed: the same codes,
//! names and declared units, because the point of the mode is to make the stand
//! demonstrate the flows the prototype demonstrates. A unit here is a real
//! metering unit the pricing rules will accept — `vCPU-hour`, `GiB-month` — so a
//! row authored from this list is a well-formed row. Only its provenance is
//! fictional.

use async_trait::async_trait;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::ports::{CatalogSku, CatalogTaxCategory, ProductCatalogClientV1};

/// The reserved id namespace every fabricated SKU is minted in.
///
/// A real registry has no reason to issue ids in it, so a `LIKE` over this
/// prefix finds every plan this mode ever touched.
pub const DEV_LOCAL_SKU_PREFIX: &str = "ddddddd";

/// The code prefix, so the fabrication is visible in the pick-list itself.
pub const DEV_LOCAL_CODE_PREFIX: &str = "DEV-";

/// `dddddddN-0000-4000-8000-0000000000NN`, one per entry, stable across boots.
///
/// Stable matters: a plan bound to a SKU keeps the binding, and an id that
/// changed on restart would leave every bound plan pointing at nothing while
/// looking fine.
fn dev_sku_id(n: u8) -> Uuid {
    // **From bytes, so it cannot fail.** Rendering the id as a string put the index
    // in a nibble-wide field: a sixteenth entry widens the first group, the parse
    // fails, and the fallback was `Uuid::nil()` — two fabricated SKUs sharing one
    // id, outside the reserved prefix the sweep in the module doc matches on. The
    // whole index rides the tail here, which is what keeps the ids distinct past
    // the nibble, and the layout reproduces the rendered form exactly for every
    // index the catalog holds.
    let mut bytes = [0_u8; 16];
    bytes[0..3].copy_from_slice(&[0xdd, 0xdd, 0xdd]);
    bytes[3] = 0xd0 | (n & 0x0f);
    bytes[6..8].copy_from_slice(&[0x40, 0x00]);
    bytes[8..10].copy_from_slice(&[0x80, 0x00]);
    bytes[10..16].copy_from_slice(&u64::from(n).to_be_bytes()[2..]);
    Uuid::from_bytes(bytes)
}

fn sku(
    n: u8,
    code: &str,
    name: &str,
    unit: Option<&str>,
    tier: Option<&str>,
    status: &str,
) -> CatalogSku {
    CatalogSku {
        sku_id: dev_sku_id(n),
        sku_code: format!("{DEV_LOCAL_CODE_PREFIX}{code}"),
        name: name.to_owned(),
        metering_unit: unit.map(str::to_owned),
        status: status.to_owned(),
        plan_tier: tier.map(str::to_owned),
    }
}

/// The fabricated catalog. See the module doc before selecting it.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalDevStaticProductCatalog;

impl LocalDevStaticProductCatalog {
    /// Demo definitions only, enabled by the same explicit opt-in as demo SKUs.
    /// Codes are stable and visibly fabricated; they encode no jurisdiction or
    /// rate. Replacing this provider does not rewrite stored price snapshots.
    #[must_use]
    pub fn tax_categories() -> Vec<CatalogTaxCategory> {
        [
            ("DEV-TAX-SUBSCRIPTION", "Subscription (demo)"),
            ("DEV-TAX-USAGE", "Usage (demo)"),
            ("DEV-TAX-SUPPORT", "Support (demo)"),
        ]
        .into_iter()
        .map(|(code, display_name)| CatalogTaxCategory {
            code: code.to_owned(),
            display_name: display_name.to_owned(),
        })
        .collect()
    }

    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// The set, built fresh per call so no caller can mutate a shared list.
    #[must_use]
    pub fn skus() -> Vec<CatalogSku> {
        vec![
            // Usage SKUs: each declares a unit, and that declaration is what
            // makes it a usage SKU — there is no separate flag, in the registry
            // PRD or here.
            sku(
                1,
                "COMP-VCPU-H",
                "Compute vCPU",
                Some("vCPU-hour"),
                Some("Pro"),
                "published",
            ),
            sku(
                2,
                "COMP-GIB-H",
                "Compute RAM",
                Some("GiB-hour"),
                Some("Pro"),
                "published",
            ),
            sku(
                3,
                "S3-STOR-GIBM",
                "Object Storage",
                Some("GiB-month"),
                Some("Standard"),
                "published",
            ),
            sku(
                4,
                "S3-OPS-1K",
                "S3 Operations",
                Some("1k-ops"),
                Some("Standard"),
                "published",
            ),
            sku(
                5,
                "NET-EGRESS-GIB",
                "External Network Traffic",
                Some("GiB-egress"),
                Some("Pro"),
                "published",
            ),
            sku(
                6,
                "COMP-CLDL-H",
                "Dynamic Cloudlet",
                Some("cloudlet-hour"),
                Some("Pro"),
                "published",
            ),
            // No unit: priced per period, never as usage.
            sku(
                7,
                "SUB-PRO-YR",
                "Pro Subscription",
                None,
                Some("Pro"),
                "published",
            ),
            sku(8, "SEAT-STD", "Team Seat", None, Some("Pro"), "published"),
            // A draft and a deprecated one, because a pick-list that only ever
            // shows publishable entries never teaches an operator that the
            // status matters.
            sku(
                9,
                "COMP-GPU-H",
                "GPU Compute",
                Some("GPU-hour"),
                Some("Pro"),
                "draft",
            ),
            sku(
                10,
                "VM-LEG-H",
                "Legacy VM",
                Some("instance-hour"),
                Some("Legacy"),
                "deprecated",
            ),
        ]
    }
}

#[async_trait]
impl ProductCatalogClientV1 for LocalDevStaticProductCatalog {
    async fn list_skus(&self, _ctx: &SecurityContext) -> Result<Vec<CatalogSku>, CanonicalError> {
        Ok(Self::skus())
    }

    async fn list_tax_categories(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<Vec<CatalogTaxCategory>, CanonicalError> {
        Ok(Self::tax_categories())
    }
}

#[cfg(test)]
#[path = "local_dev_catalog_tests.rs"]
mod local_dev_catalog_tests;
