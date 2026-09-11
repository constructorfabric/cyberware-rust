//! `ClusterCapabilities` integration: event notification via
//! `ClusterCacheV1::put` + `watch` (`DESIGN.md:604-605,756-758`), and
//! `DirectoryService` advertise-address resolution
//! (`eb-dispatcher-routing`).

mod advertise_address;

pub(crate) use advertise_address::{AdvertiseAddressResolver, ConfigAdvertiseAddress};
