//! A **locally invented** `CatalogVersion` source, for deployments that have no
//! registry gear to talk to.
//!
//! # This is the second incrementer the SDK warns about, on purpose
//!
//! [`CatalogVersionRegistryV1`](crate::domain::ports::CatalogVersionRegistryV1)
//! says the Product & SKU registry is the **sole** incrementer, and that a version
//! the catalog invented locally "would make it a second incrementer, and two
//! incrementers make `CatalogVersion` unordered". That is exactly what this type
//! is. It exists because the registry gear is not in this repository yet, and
//! without *some* source every publish answers
//! `503 no catalog-version registry configured` — which takes the whole
//! publish-dependent half of the gear (windows, cutovers, supersessions,
//! repricing, migrations) out of reach, including on a stand meant to demonstrate
//! it.
//!
//! So the trade is stated rather than hidden: **a deployment that selects this is
//! choosing versions no registry issued.** The moment a real registry exists, any
//! version minted here is a version it never handed out, and two orderings meet.
//! Nothing here is safe to run beside a real registry, and nothing here should
//! outlive one.
//!
//! Three properties keep that trade auditable rather than invisible:
//!
//! * **It is opt-in by a value that says what it is.** Not a `true`: the config
//!   takes `mode = "local_dev_invented_versions"`, which cannot be set by
//!   accident and reads, in the deployment's own file, as the admission it is.
//! * **Every ref it mints is marked.** A pending ref is
//!   `dev-local-v{version}`, so `pricing_plan_revision.pending_version_ref` and
//!   every row that carried one can be swept for contamination later with a
//!   `LIKE 'dev-local-%'`. A registry-issued ref has no reason to look like this.
//! * **It says so on every boot**, at `warn!`, naming the mode. An operator who
//!   inherits the stand finds out from the log rather than from a version
//!   collision.
//!
//! # Why the clock and not a counter
//!
//! The version must be monotonic (`CatalogVersion` — "monotonic per tenant"), and
//! the two obvious counters are both worse:
//!
//! * **An in-memory counter re-issues versions after a restart**, which is worse
//!   than unordered — two different publishes would carry the same version and be
//!   indistinguishable rather than merely mis-sorted.
//! * **A counter in the gear's own tables** would need a migration, and the
//!   deployment this unblocks holds a live catalog. Adding schema to reach a demo
//!   is the wrong risk.
//!
//! Wall-clock milliseconds are monotonic across restarts without storing
//! anything, and a global sequence is monotonic per tenant by construction. The
//! one thing the clock does not give is strictness under load — two calls inside
//! one millisecond would collide — so the last issued value is held and the next
//! is `max(now, last + 1)`. That makes the sequence strictly increasing whatever
//! the clock does, including a backwards NTP step.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};

use async_trait::async_trait;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use bss_pricing_sdk::catalog_version::CatalogVersion;

use crate::domain::instant::timestamp_millis;
use crate::domain::ports::{CatalogVersionRegistryV1, PendingVersionRef, registry_rejected};
use time::OffsetDateTime;

/// The prefix every ref this type mints carries, so its artifacts stay findable.
pub const DEV_LOCAL_REF_PREFIX: &str = "dev-local-v";

/// How many answered `request_id`s the retry window holds.
///
/// The map exists to make a **retry** idempotent, and a retry follows its request
/// by seconds. Keeping every id for the life of the process bought nothing past
/// that window and grew without bound — one entry per publish, cutover, window
/// mutation and membership act, for as long as the stand runs.
///
/// Evicting the oldest is the same divergence the map already documents for a
/// restart, bounded and deliberate: a retry of a request 4096 mints old answers a
/// *new* version rather than replaying the first. `committed_version` still
/// resolves both refs, because the version travels inside the ref rather than in
/// this map.
const RETRY_WINDOW: usize = 4096;

/// The guarded state: the answered-request map and the version counter.
#[derive(Debug, Default)]
struct Issued {
    /// `request_id` -> the version it was already answered, so a retry is
    /// idempotent as the contract requires.
    ///
    /// In memory, and therefore **only within one process lifetime**: after a
    /// restart the same `request_id` mints a *new* version rather than replaying
    /// the old one. That is a real divergence from the contract, and it is
    /// accepted here rather than papered over — persisting it is the migration
    /// this type exists to avoid. It costs a duplicate version on the narrow path
    /// where a publish is retried across a restart, and
    /// [`LocalDevCatalogVersionRegistryV1::committed_version`] still resolves both
    /// refs because the version travels *inside* the ref.
    ///
    /// Bounded to [`RETRY_WINDOW`]; see its doc.
    by_request: HashMap<String, u64>,
    /// The ids in the order they were answered, so the oldest can be evicted
    /// once the window is full. A `VecDeque` rather than an LRU crate: the
    /// eviction order that matters here is *mint* order, because a retry
    /// follows its own request and never an older one.
    order: VecDeque<String>,
    /// **The highest version ever issued**, carried rather than recomputed.
    ///
    /// `next_version` used to answer `by_request.values().max()`, which is
    /// O(entries) on every mint and taken under the lock — the cost this field
    /// removes.
    ///
    /// It is also what keeps the mint independent of the eviction policy. The
    /// maximum would still be *derivable* from a bounded map while eviction is
    /// FIFO by mint order, because that removes the lowest versions first — but
    /// then the sequence's strictness would rest on a property of the eviction
    /// order rather than on anything the mint states, and changing that order to
    /// a true LRU would silently re-issue a version already handed out. That is
    /// the one failure this type's own tests call worse than being unordered, so
    /// it is a field.
    high_water: u64,
}

/// A `CatalogVersion` source that invents versions locally. See the module docs
/// for why that is a hazard and why it is nonetheless offered.
#[derive(Debug, Default)]
pub struct LocalDevCatalogVersionRegistryV1 {
    issued: Mutex<Issued>,
}

impl LocalDevCatalogVersionRegistryV1 {
    /// A fresh source with nothing issued yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The next strictly-increasing version, whatever the clock did.
    ///
    /// O(1): the high-water mark is a field. See [`Issued::high_water`] for why it
    /// is carried rather than read off the map.
    fn next_version(issued: &Issued) -> u64 {
        let now = u64::try_from(timestamp_millis(OffsetDateTime::now_utc()).max(0)).unwrap_or(0);
        now.max(issued.high_water.saturating_add(1))
    }

    /// The ref a version travels in.
    fn ref_for(version: u64) -> String {
        format!("{DEV_LOCAL_REF_PREFIX}{version}")
    }
}

#[async_trait]
impl CatalogVersionRegistryV1 for LocalDevCatalogVersionRegistryV1 {
    async fn request_version(
        &self,
        _ctx: &SecurityContext,
        request_id: &str,
    ) -> Result<PendingVersionRef, CanonicalError> {
        // `parking_lot::Mutex` per `guidelines/DEPENDENCIES.md`: its `lock()`
        // returns the guard directly, so there is no poison arm to map. The
        // guard is never held across an `await`.
        let mut issued = self.issued.lock();

        // Idempotent on `request_id`: a retry after a crash must not mint a
        // second version. The contract is explicit that this is the whole point
        // of the handle.
        let version = if let Some(already) = issued.by_request.get(request_id) {
            *already
        } else {
            let minted = Self::next_version(&issued);
            issued.high_water = minted;
            issued.by_request.insert(request_id.to_owned(), minted);
            issued.order.push_back(request_id.to_owned());
            // The window, held from the back. `while` rather than `if` so a
            // lowered `RETRY_WINDOW` drains rather than staying one over forever.
            while issued.order.len() > RETRY_WINDOW {
                if let Some(evicted) = issued.order.pop_front() {
                    issued.by_request.remove(&evicted);
                }
            }
            minted
        };

        Ok(PendingVersionRef {
            request_id: request_id.to_owned(),
            pending_ref: Self::ref_for(version),
        })
    }

    async fn committed_version(
        &self,
        _ctx: &SecurityContext,
        pending_ref: &str,
    ) -> Result<Option<CatalogVersion>, CanonicalError> {
        // **Committed the instant it is asked**, and deliberately so: a real
        // registry MAY batch, and answering `None` for a while would model that
        // faithfully — but this type exists to let a publish complete, and a
        // pending ref that never commits leaves the revision unaddressable, which
        // is the state it was brought in to end.
        //
        // Parsed out of the ref rather than looked up, so it survives a restart:
        // the projector resolves refs written by a previous process, and a map
        // consulted here would answer `None` for all of them and strand every
        // revision published before the last boot.
        let Some(digits) = pending_ref.strip_prefix(DEV_LOCAL_REF_PREFIX) else {
            // Not one of ours. `Rejected` rather than `None`: `None` means "not
            // committed yet" and would have the caller wait forever for a ref
            // this source can never commit.
            return Err(registry_rejected(format!(
                "not a locally minted pending ref: {pending_ref}"
            )));
        };
        let version = digits.parse::<u64>().map_err(|e| {
            registry_rejected(format!(
                "locally minted ref carries no version: {pending_ref} ({e})"
            ))
        })?;
        Ok(Some(CatalogVersion::new(version)))
    }
}

#[cfg(test)]
#[path = "local_dev_registry_tests.rs"]
mod local_dev_registry_tests;
