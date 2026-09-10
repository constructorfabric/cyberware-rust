//! Credential-store domain service (ADR-0006: immutable value versions).
//!
//! Orchestrates authorization, typed-secret validation, hierarchy resolution,
//! the immutable-value write/read/delete protocols, fingerprint verification,
//! and the periodic maintenance job's garbage-collection logic.

use std::sync::Arc;
use std::time::{Duration, Instant};

use authz_resolver_sdk::PolicyEnforcer;
use credstore_sdk::{
    CredStoreError, CredStorePluginClientV1, GetSecretResponse, OwnerId, SecretRef, SecretValue,
    SharingMode, TenantId, ValueId,
};
use tokio::time::sleep;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use authz_resolver_sdk::pep::ResourceType;

use crate::domain::authz::{self, actions, scope_for};
use crate::domain::error::DomainError;
use crate::domain::ports::metrics::{
    CredStoreMetricsPort, Dep, DepOp, FenceVerify, Outcome, ReadOutcome,
};
use crate::domain::ports::plugin::PluginSelector;
use crate::domain::resolver::TenantDirectory;
use credstore_sdk::SecretType;
use time::OffsetDateTime;

use crate::domain::secret::fence;
use crate::domain::secret::model::{
    GcEntry, GcReason, NewSecret, SecretRow, WritePrecondition, WriteSpec,
};
use crate::domain::secret::repo::SecretRepo;
use crate::domain::secret::type_resolver::{ResolvedSecretType, SecretTypeResolver};
use crate::domain::secret::typing;

/// Maps a [`CredStoreError`] from the plugin layer to a [`DomainError`].
#[must_use]
pub fn map_plugin_err(e: CredStoreError) -> DomainError {
    match e {
        CredStoreError::NotFound => DomainError::NotFound,
        CredStoreError::AccessDenied => DomainError::AccessDenied { cause: None },
        CredStoreError::ServiceUnavailable {
            detail,
            retry_after,
        } => {
            // The plugin's own detail may embed backend/infra specifics (a
            // future vault-backed plugin's error text is not curated for the
            // credstore boundary the way a CF-internal sibling's is, and could
            // in principle carry sensitive material — e.g. echo a token being
            // written). The wire already redacts it; for the same reason we do
            // NOT log the raw detail either (a credential store must not risk
            // secret material in its logs). We record only its length so
            // operators can still tell an empty detail from a populated one
            // when correlating an outage.
            tracing::warn!(
                detail_len = detail.len(),
                "credstore: storage plugin reported unavailable (detail redacted)"
            );
            DomainError::ServiceUnavailable {
                detail: "storage backend unavailable".to_owned(),
                retry_after,
                cause: None,
            }
        }
        // Operator misconfiguration, not a transient outage: keep a stable,
        // distinguishable detail and no `retry_after` so callers don't retry.
        CredStoreError::NoPluginAvailable => DomainError::ServiceUnavailable {
            detail: "no storage plugin registered".into(),
            retry_after: None,
            cause: None,
        },
        CredStoreError::Conflict => DomainError::Conflict,
        // These are plugin contract violations (a plugin should never return
        // them for get/put/delete on this SPI). Their free-text payloads
        // originate in the plugin, and — like the `ServiceUnavailable` detail
        // above — a future non-CF backend's error text is not curated for the
        // credstore boundary and could carry secret material. A credential
        // store must not risk that in its own logs, so we drop the raw text
        // and keep only its length as an internal diagnostic.
        CredStoreError::InvalidSecretRef { reason } => DomainError::Internal {
            diagnostic: format!(
                "plugin returned InvalidSecretRef (detail redacted, {} bytes)",
                reason.len()
            ),
            cause: None,
        },
        CredStoreError::UnsupportedTransition { detail } => DomainError::Internal {
            diagnostic: format!(
                "plugin returned UnsupportedTransition (detail redacted, {} bytes)",
                detail.len()
            ),
            cause: None,
        },
        CredStoreError::TypeViolation { reason, detail } => DomainError::Internal {
            diagnostic: format!(
                "plugin returned TypeViolation (detail redacted, {} bytes)",
                reason.len() + detail.len()
            ),
            cause: None,
        },
        CredStoreError::Internal(s) => DomainError::Internal {
            diagnostic: format!(
                "plugin returned Internal error (detail redacted, {} bytes)",
                s.len()
            ),
            cause: None,
        },
    }
}

/// Garbage-collection settings for the periodic maintenance job (`credstore
/// gc`), from `GcCfg`. The gear's own lifecycle runs no timer of any kind
/// (ADR-0006 D7): these settings only bound [`Service::run_gc`]'s batches and
/// its pending-reclaim age threshold, whenever an operator or scheduler
/// invokes it.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct GcSettings {
    pub pending_max_age_secs: u64,
    pub batch_size: u64,
}

/// Outcome of one [`Service::run_gc`] invocation.
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Expired `active` rows removed (§6.4 pass 1).
    pub expired_deleted: u64,
    /// Versions deleted by the gc drain (`reason != pending`).
    pub gc_deleted: u64,
    /// Orphaned `pending` versions reclaimed (older than
    /// `pending_max_age_secs`, unreferenced by any row).
    pub gc_pending_reclaimed: u64,
}

/// Re-read the cached fence key at least this often. Bounds the window in which
/// a replica that adopted a losing key during a bootstrap race keeps using it:
/// it converges on the stored key within this TTL even if it never hits a
/// verify mismatch. The backend read is cheap and the key almost never changes.
const FENCE_KEY_CACHE_TTL: Duration = Duration::from_mins(1);

/// Minimum spacing between fingerprint-mismatch–triggered backend re-reads of
/// the fence key. Under ADR-0006 a mismatch means the backend entry the row
/// points to was altered or corrupted out of band — a by-design, fail-closed
/// poison until a fresh write recovers it — so without this every read of
/// such a row would re-read the key from the backend. The cooldown bounds
/// those forced reloads to one per window per replica. It gates on the *last
/// forced reload* (not cache age), so the first mismatch after a quiet window
/// always re-reads and heals a genuinely stale key; only the repeated,
/// never-healing poison reads are suppressed.
const FENCE_KEY_REFRESH_COOLDOWN: Duration = Duration::from_secs(15);

/// The in-process fence key, shared out of the cache. Zeroizing so the
/// process's single highest-value secret is wiped from the heap on drop.
type FenceKey = Arc<zeroize::Zeroizing<Vec<u8>>>;

/// Cached fence key plus when it was loaded, so [`Service::fence_key`] can
/// re-read it from the backend after [`FENCE_KEY_CACHE_TTL`] and converge on
/// the stored key even without a verify mismatch to trigger a refresh.
#[domain_model]
struct CachedFenceKey {
    key: FenceKey,
    loaded_at: Instant,
}

/// `CredStore` domain service — get / put / delete with walk-up, the
/// immutable-value write protocol, and `AuthZ`.
#[domain_model]
pub struct Service {
    repo: Arc<dyn SecretRepo>,
    dir: Arc<dyn TenantDirectory>,
    enforcer: PolicyEnforcer,
    plugins: Arc<dyn PluginSelector>,
    types: Arc<dyn SecretTypeResolver>,
    metrics: Arc<dyn CredStoreMetricsPort>,
    gc: GcSettings,
    /// In-process cache of the value-fingerprint fence key (auto-generated,
    /// stored in the value-store backend under `(TenantId::nil(),
    /// FENCE_KEY_VALUE_ID)`). A fingerprint mismatch triggers a
    /// cooldown-gated, single-flighted backend re-read (swapped in place,
    /// never evicted to `None`) so a replica holding a stale key self-heals
    /// without a poisoned row thrashing the cache. Never logged, never on the
    /// wire. Wrapped in [`zeroize::Zeroizing`] so the process's single
    /// highest-value secret (it fingerprints every stored value) is wiped
    /// from the heap when the cache is replaced or the process exits,
    /// matching how every `SecretValue` in the system zeroizes on drop.
    fence_key: std::sync::RwLock<Option<CachedFenceKey>>,
    /// Serialises fence-key (re)load on a single replica so concurrent first
    /// writers don't each run the bootstrap and race one another locally; the
    /// jitter protocol in [`Service::load_fence_key`] then only has to defend
    /// against *inter*-replica races.
    bootstrap_lock: tokio::sync::Mutex<()>,
    /// When the last fingerprint-mismatch–triggered *forced* reload of the
    /// fence key ran, gating the next one to [`FENCE_KEY_REFRESH_COOLDOWN`].
    /// `None` until the first forced reload, so the first mismatch always
    /// re-reads (healing a stale key); thereafter a poisoned row cannot drive
    /// more than one backend re-read per window. Independent of the cache's own
    /// load time.
    last_fence_refresh: std::sync::Mutex<Option<Instant>>,
}

impl Service {
    /// Creates a new [`Service`].
    #[must_use]
    pub fn new(
        repo: Arc<dyn SecretRepo>,
        dir: Arc<dyn TenantDirectory>,
        enforcer: PolicyEnforcer,
        plugins: Arc<dyn PluginSelector>,
        types: Arc<dyn SecretTypeResolver>,
        metrics: Arc<dyn CredStoreMetricsPort>,
        gc: GcSettings,
    ) -> Self {
        Self {
            repo,
            dir,
            enforcer,
            plugins,
            types,
            metrics,
            gc,
            fence_key: std::sync::RwLock::new(None),
            bootstrap_lock: tokio::sync::Mutex::new(()),
            last_fence_refresh: std::sync::Mutex::new(None),
        }
    }

    /// Evaluate the PDP scope for `action`, recording it as a timed dependency.
    ///
    /// The PDP call gates every get/put/delete and drives 503s, so it is timed
    /// like the plugin and tenant-resolver dependencies.
    async fn scope_for_timed(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        action: &str,
    ) -> Result<AccessScope, DomainError> {
        let t0 = Instant::now();
        let result = scope_for(&self.enforcer, ctx, resource, action).await;
        // A PDP *denial* (`AccessDenied`) is a normal authorization decision —
        // the dependency answered — not a health signal; counting it as an
        // error would inflate the PDP error rate and risk false outage alerts.
        // Only an evaluation failure/outage is a dependency error. Mirrors the
        // domain/transport split the type resolver makes.
        let outcome = match &result {
            Ok(_) | Err(DomainError::AccessDenied { .. }) => Outcome::Success,
            Err(_) => Outcome::Error,
        };
        self.metrics.dependency(
            Dep::Pdp,
            DepOp::Evaluate,
            outcome,
            t0.elapsed().as_secs_f64(),
        );
        result
    }

    /// Resolve the type of an **existing** row (`secret_type_uuid` was
    /// validated when the row was written). An `UNKNOWN_SECRET_TYPE`
    /// violation here means the type was deregistered while rows persist —
    /// an operational inconsistency, not a caller error — so it is remapped
    /// onto a retryable 503; registry outages propagate as 503 already.
    async fn resolve_stored(&self, type_uuid: Uuid) -> Result<ResolvedSecretType, DomainError> {
        match self.types.resolve(type_uuid).await {
            Err(DomainError::TypeViolation { detail, .. }) => {
                tracing::warn!(
                    uuid = %type_uuid,
                    detail = %detail,
                    "stored secret type no longer resolves in the types-registry"
                );
                Err(DomainError::ServiceUnavailable {
                    detail: "secret type is not resolvable".to_owned(),
                    retry_after: None,
                    cause: None,
                })
            }
            other => other,
        }
    }

    // ── Value-fingerprint fence (ADR-0003, narrowed by ADR-0006) ────────────

    /// Internal service context for fence-key backend operations. Nil subject
    /// and nil tenant: plugins are pure value stores keyed by the explicit
    /// arguments, and no external caller ever carries the nil tenant, which is
    /// what keeps the reserved key unreachable through the API.
    fn fence_ctx() -> Result<(SecurityContext, TenantId, ValueId), DomainError> {
        let ctx = SecurityContext::builder()
            .subject_id(Uuid::nil())
            .subject_tenant_id(Uuid::nil())
            .build()
            .map_err(|e| {
                DomainError::internal(format!("fence: failed to build service context: {e}"))
            })?;
        Ok((ctx, TenantId::nil(), credstore_sdk::FENCE_KEY_VALUE_ID))
    }

    /// The cached fence key, loading (and on first boot generating) it from the
    /// value-store backend when the cache is cold or older than
    /// [`FENCE_KEY_CACHE_TTL`]. The TTL re-read lets a replica that adopted a
    /// losing key during a bootstrap race converge on the stored key without
    /// waiting for a verify mismatch.
    async fn fence_key(
        &self,
        plugin: &Arc<dyn CredStorePluginClientV1>,
    ) -> Result<FenceKey, DomainError> {
        {
            let guard = self
                .fence_key
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = guard.as_ref()
                && cached.loaded_at.elapsed() < FENCE_KEY_CACHE_TTL
            {
                return Ok(Arc::clone(&cached.key));
            }
        }
        self.load_fence_key(plugin, false).await
    }

    /// Re-read the fence key from the backend — the self-heal a fingerprint
    /// mismatch triggers, so a replica whose cache went stale (key re-created
    /// under it) converges without restart.
    async fn refresh_fence_key(
        &self,
        plugin: &Arc<dyn CredStorePluginClientV1>,
    ) -> Result<FenceKey, DomainError> {
        self.load_fence_key(plugin, true).await
    }

    /// Whether a forced fence-key reload ran within [`FENCE_KEY_REFRESH_COOLDOWN`].
    /// `false` until the first forced reload, so an initial mismatch is never
    /// suppressed.
    fn fence_refresh_within_cooldown(&self) -> bool {
        self.last_fence_refresh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|at| at.elapsed() < FENCE_KEY_REFRESH_COOLDOWN)
    }

    /// (Re)load the fence key from the backend's reserved entry
    /// `(TenantId::nil(), FENCE_KEY_VALUE_ID)`.
    ///
    /// The plugin port has no atomic create-if-absent, so first-boot generation
    /// cannot be a true single-writer operation. Instead we make a bootstrap
    /// race both **unlikely** and **low-impact**:
    ///
    /// * A per-replica [`Self::bootstrap_lock`] serialises this so concurrent
    ///   first writers on one replica don't race each other locally.
    /// * When the key is absent we jitter, re-check, and only then generate and
    ///   `put`. Under ADR-0006 a plugin may reject a `put` to an id it already
    ///   holds with `Conflict` — that is exactly "another replica won; re-read",
    ///   not an error, since the fence key's `value_id` is a fixed constant a
    ///   concurrent bootstrap on another replica may have just claimed.
    /// * After publishing (or losing the race to a `Conflict`) we jitter again
    ///   and **adopt whatever is stored** — ours if we won, a racing writer's
    ///   if theirs landed first — rather than re-`put`ting. The cache is
    ///   populated only from this settle read, so no value is ever stamped
    ///   with a candidate that hasn't survived it.
    ///
    /// The key has no metadata row, so it is invisible to every API path. Any
    /// residual divergence is fail-closed: a wrong key only ever yields a
    /// fingerprint mismatch (404, healed by the next ordinary write), never a
    /// value served under foreign metadata.
    async fn load_fence_key(
        &self,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        force: bool,
    ) -> Result<FenceKey, DomainError> {
        let _bootstrap = self.bootstrap_lock.lock().await;
        // Reuse the cache rather than re-reading the backend when: a non-forced
        // load finds it still within the TTL, or a forced refresh finds a prior
        // forced reload within the cooldown (re-reading sooner cannot heal a
        // poisoned row). A concurrent caller may also have just reloaded while
        // we waited on the lock — this same check picks that up.
        {
            let guard = self
                .fence_key
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = guard.as_ref() {
                let reuse = if force {
                    self.fence_refresh_within_cooldown()
                } else {
                    cached.loaded_at.elapsed() < FENCE_KEY_CACHE_TTL
                };
                if reuse {
                    return Ok(Arc::clone(&cached.key));
                }
            }
        }
        // About to re-read on a mismatch: stamp the forced-reload clock now so
        // repeated poisoned mismatches (and concurrent ones behind the lock)
        // coalesce, even if the read below fails.
        if force {
            *self
                .last_fence_refresh
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
        }

        let (ctx, tenant, value_id) = Self::fence_ctx()?;
        // Fast path: the key already exists (every start after the first, and
        // most concurrent cold starts once one replica has written it).
        if let Some(v) = plugin
            .get(&ctx, &tenant, &value_id)
            .await
            .map_err(map_plugin_err)?
        {
            return Ok(self.store_fence_key(zeroize::Zeroizing::new(v.as_bytes().to_vec())));
        }

        // Cold-start bootstrap. Jitter, then re-check: a peer may have won.
        Self::bootstrap_jitter().await;
        if let Some(v) = plugin
            .get(&ctx, &tenant, &value_id)
            .await
            .map_err(map_plugin_err)?
        {
            return Ok(self.store_fence_key(zeroize::Zeroizing::new(v.as_bytes().to_vec())));
        }

        // Still absent: publish a candidate, settle, then adopt what landed.
        // Keep the material in zeroizing buffers so no plain `Vec<u8>` copy
        // lingers on the heap after this scope.
        let candidate = zeroize::Zeroizing::new(fence::generate_key()?);
        match plugin
            .put(
                &ctx,
                &tenant,
                &value_id,
                SecretValue::new(candidate.to_vec()),
            )
            .await
        {
            // A `Conflict` here means a stricter plugin's immutability guard
            // rejected our put because another replica's candidate landed
            // first between our re-check and this call — exactly the
            // residual race `load_fence_key`'s docs describe. Not an error:
            // fall through to the settle read either way.
            Ok(()) | Err(CredStoreError::Conflict) => {}
            Err(e) => return Err(map_plugin_err(e)),
        }
        Self::bootstrap_jitter().await;
        let stored = plugin
            .get(&ctx, &tenant, &value_id)
            .await
            .map_err(map_plugin_err)?
            // Our own write vanishing is not expected; fall back to the
            // candidate so bootstrap still makes progress (fail-closed).
            .map_or(candidate, |v| {
                zeroize::Zeroizing::new(v.as_bytes().to_vec())
            });
        Ok(self.store_fence_key(stored))
    }

    /// Publish `key_bytes` as the cached fence key, timestamped for the TTL
    /// re-read, and hand back the shared handle.
    fn store_fence_key(&self, key_bytes: zeroize::Zeroizing<Vec<u8>>) -> FenceKey {
        let key: FenceKey = Arc::new(key_bytes);
        *self
            .fence_key
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CachedFenceKey {
            key: Arc::clone(&key),
            loaded_at: Instant::now(),
        });
        key
    }

    /// Sleep a random `0..=FENCE_BOOTSTRAP_JITTER_MAX_MS` to de-synchronise
    /// replicas racing the first-boot fence-key generation.
    async fn bootstrap_jitter() {
        use rand::RngExt as _;
        let ms = rand::rng().random_range(0..=fence::BOOTSTRAP_JITTER_MAX_MS);
        sleep(Duration::from_millis(ms)).await;
    }

    /// Fingerprint `value` under the current fence key (write-path stamping).
    /// Computed from a borrow so the caller can still move `value` into
    /// `plugin.put` afterward (`SecretValue` is not `Clone` by design).
    async fn stamp_fp(
        &self,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        value: &SecretValue,
    ) -> Result<Vec<u8>, DomainError> {
        let key = self.fence_key(plugin).await?;
        Ok(fence::compute_fp(key.as_slice(), value.as_bytes()))
    }

    /// Retrieve a secret, walking up the tenant hierarchy.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::AccessDenied`] if the caller is out of scope.
    /// Returns [`DomainError::NotFound`] if the plugin has no value for a
    /// resolved row's current `value_id` even after one retry against the
    /// row's re-read, current pointer.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, DomainError> {
        let req = TenantId(ctx.subject_tenant_id());
        let subject = OwnerId(ctx.subject_id());
        let chain = self.dir.ancestor_chain(ctx, req).await?;

        // Resolve first (prefetch — AUTHZ_USAGE_SCENARIOS S09): a secret that does
        // not resolve is a 404 without consulting the PDP; there is nothing to
        // authorize.
        let Some(row) = self.repo.resolve_for_get(req, subject, key, &chain).await? else {
            self.metrics.read_outcome(ReadOutcome::Miss);
            return Ok(None);
        };

        // Single PDP evaluation, on the secret's full concrete type (including
        // `generic`) as resolved from the types-registry, gated on the
        // *caller's* tenant. Hierarchical visibility (a shared secret
        // inherited from an ancestor) is decided by the resolver above, so
        // the gate uses `req`, not the row's owner tenant — this is what lets
        // inherited reads work. A PDP denial or an out-of-scope tenant is
        // indistinguishable from a missing secret (anti-enumeration 404); a
        // PDP or registry *outage* propagates as 503.
        let resolved = self.resolve_stored(row.secret_type_uuid).await?;
        let scope = match self
            .scope_for_timed(
                ctx,
                &authz::secret_type_resource(&resolved.gts_id),
                actions::READ,
            )
            .await
        {
            Ok(scope) => scope,
            Err(DomainError::AccessDenied { .. }) => {
                self.metrics.read_outcome(ReadOutcome::Miss);
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if !self.repo.scope_includes_tenant(&scope, req.0).await? {
            self.metrics.cross_tenant_denied();
            self.metrics.read_outcome(ReadOutcome::Miss);
            return Ok(None);
        }

        let depth = chain
            .iter()
            .position(|c| *c == row.tenant_id.0)
            .unwrap_or(chain.len());
        self.metrics.walkup_depth(depth as u64);

        let plugin = self.plugins.resolve().await?;

        // A resolved row with no value_id ("declared") is never returned by
        // resolve_for_get in Phase 1 (its predicate stays status = active),
        // but guard the invariant explicitly rather than unwrap a None.
        let Some(value_id) = row.value_id else {
            return Err(DomainError::NotFound);
        };

        let Some((value, served_row)) = self
            .fetch_with_retry(&plugin, ctx, req, subject, key, &chain, &row, value_id)
            .await?
        else {
            self.metrics.read_outcome(ReadOutcome::Miss);
            return Ok(None);
        };

        // Fence verification: the value is served only when its fingerprint
        // matches the row that names it. Under immutable versions a mismatch
        // can only mean the backend entry the row points to was altered or
        // corrupted out of band (the torn-write cause is structurally
        // impossible: step 3 of a write — the backend put — always precedes
        // step 4 — the row CAS — so a row never points at a value_id until
        // the bytes under it are already fully written).
        let stored_fp = served_row.value_fp.as_ref().ok_or_else(|| {
            // A row with a value_id but no fingerprint cannot exist by
            // construction (`ck_credstore_fp_with_value`); treat it as
            // corruption, never a panic.
            DomainError::Internal {
                diagnostic: "row has value_id but no value_fp (constraint invariant broken)"
                    .to_owned(),
                cause: None,
            }
        })?;
        let fkey = self.fence_key(&plugin).await?;
        let mut fp_ok = fence::verify_fp(fkey.as_slice(), value.as_bytes(), stored_fp);
        if !fp_ok {
            // One-shot key refresh: a replica whose cached key went stale
            // (key re-created under it) self-heals before failing closed.
            let fkey = self.refresh_fence_key(&plugin).await?;
            fp_ok = fence::verify_fp(fkey.as_slice(), value.as_bytes(), stored_fp);
        }
        if fp_ok {
            self.metrics.fence_verify(FenceVerify::Ok);
        } else {
            self.metrics.fence_verify(FenceVerify::Mismatch);
            self.metrics.read_outcome(ReadOutcome::Miss);
            tracing::warn!(
                tenant = %served_row.tenant_id.0,
                key = %key.as_ref(),
                "credstore get: value fingerprint mismatch; failing closed \
                 (the backend entry the row points to was altered or corrupted out of band; \
                 a fresh write recovers it)"
            );
            return Ok(None);
        }

        let is_inherited = served_row.tenant_id != req;
        self.metrics.read_outcome(if is_inherited {
            ReadOutcome::HitInherited
        } else {
            ReadOutcome::HitOwn
        });

        Ok(Some(GetSecretResponse {
            value,
            id: served_row.id,
            owner_tenant_id: served_row.tenant_id,
            sharing: served_row.sharing,
            is_inherited,
            version: served_row.version,
            secret_type: resolved.gts_id,
            expires_at: served_row.expires_at,
        }))
    }

    /// Read the backend value for `row`'s `value_id`, retrying once against a
    /// freshly re-resolved row if the plugin reports `NotFound` — the read
    /// landed a moment before a concurrent write switched the pointer and
    /// cleaned up the old version. A second consecutive `NotFound` is a real
    /// error (the version the row currently names is itself missing), mapped
    /// onto [`DomainError::NotFound`].
    ///
    /// The retry re-runs the *same* `resolve_for_get` call (same requesting
    /// tenant/subject/key/chain) rather than looking up the row by id, per
    /// ADR-0006's read protocol. If it comes back with a **different** row
    /// (a different generation entirely — the original was deleted and a
    /// different secret now resolves), this fails closed as `NotFound`
    /// instead of serving a value the type/PDP check above never authorized:
    /// the retry is for "the pointer moved", not "authorize a new row
    /// mid-read". Since a row's type is immutable for its lifetime, a
    /// matching id means the caller's earlier type resolution and PDP scope
    /// still apply to the retried read.
    ///
    /// Returns `Ok(None)` (with the plugin dependency metric already
    /// recorded) when the *final* attempt reports `AccessDenied` — folded
    /// into the anti-enumeration miss like a PDP denial.
    #[allow(
        clippy::too_many_arguments,
        reason = "carries the full resolve_for_get key plus the row/value_id being retried"
    )]
    async fn fetch_with_retry(
        &self,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        ctx: &SecurityContext,
        req: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        chain: &[Uuid],
        row: &SecretRow,
        value_id: ValueId,
    ) -> Result<Option<(SecretValue, SecretRow)>, DomainError> {
        match self
            .plugin_get_timed(plugin, ctx, &row.tenant_id, value_id)
            .await
        {
            Ok(Some(v)) => Ok(Some((v, row.clone()))),
            Err(DomainError::AccessDenied { .. }) => Ok(None),
            Err(e) => Err(e),
            Ok(None) => {
                let fresh = self.repo.resolve_for_get(req, subject, key, chain).await?;
                let Some(fresh) = fresh else {
                    return Err(DomainError::NotFound);
                };
                if fresh.id != row.id {
                    // A different generation now resolves; the earlier
                    // type/PDP authorization does not necessarily apply.
                    return Err(DomainError::NotFound);
                }
                let Some(fresh_value_id) = fresh.value_id else {
                    return Err(DomainError::NotFound);
                };
                match self
                    .plugin_get_timed(plugin, ctx, &fresh.tenant_id, fresh_value_id)
                    .await
                {
                    Ok(Some(v)) => Ok(Some((v, fresh))),
                    Ok(None) => Err(DomainError::NotFound),
                    Err(DomainError::AccessDenied { .. }) => Ok(None),
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Timed `plugin.get`, recording the dependency metric.
    async fn plugin_get_timed(
        &self,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        ctx: &SecurityContext,
        tenant: &TenantId,
        value_id: ValueId,
    ) -> Result<Option<SecretValue>, DomainError> {
        let t0 = Instant::now();
        let result = plugin
            .get(ctx, tenant, &value_id)
            .await
            .map_err(map_plugin_err);
        let secs = t0.elapsed().as_secs_f64();
        let outcome = match &result {
            Ok(Some(_)) => Outcome::Success,
            Ok(None) => Outcome::NotFound,
            Err(_) => Outcome::Error,
        };
        self.metrics
            .dependency(Dep::Plugin, DepOp::PluginGet, outcome, secs);
        result
    }

    /// Create or update a secret.
    ///
    /// A write targets the row of its own sharing class — `private` →
    /// `(tenant, ref, owner)`, `tenant`/`shared` → `(tenant, ref)` — so a private
    /// and a tenant/shared secret coexist under one reference (per design §4.1);
    /// a write of one class never affects the other.
    ///
    /// Every write that carries a value follows ADR-0006's single protocol
    /// regardless of precondition: mint a fresh `value_id`, record its intent,
    /// write the backend, then one transaction switches the row's pointer
    /// (`insert_active` on create, `switch_value` on overwrite).
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Conflict`] if `spec.create_only` and a secret of
    /// the same sharing class already exists.
    /// Returns [`DomainError::TypeViolation`] on a trait violation.
    #[allow(
        clippy::cognitive_complexity,
        clippy::too_many_lines,
        reason = "saga orchestration (validate -> scope -> resolve -> backend -> commit) is \
                  inherently branchy; kept as one function for readability of the flow"
    )]
    pub async fn put(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        value: SecretValue,
        spec: WriteSpec,
    ) -> Result<(), DomainError> {
        let WriteSpec {
            sharing,
            create_only,
            precondition,
            opts,
            preserve_sharing,
        } = spec;
        let tenant = TenantId(ctx.subject_tenant_id());
        let owner = OwnerId(ctx.subject_id());

        // Updates must state their concurrency stance — a version validator
        // (read-modify-write CAS) or an explicit `Exists` (last-writer-wins).
        // Create is the only preconditionless write.
        if !create_only && precondition.is_none() {
            return Err(DomainError::PreconditionRequired {
                detail: "update requires an If-Match precondition (a version validator or `*`)"
                    .to_owned(),
            });
        }

        // Fail fast if no plugin is available before touching metadata.
        let plugin = self.plugins.resolve().await?;

        // When `sharing` was omitted (`preserve_sharing`), a value rotation
        // targets the secret the owner would GET: a `private` secret is
        // owner-scoped and wins over a coexisting non-private one, so rotate the
        // private row if the caller has one under this reference; otherwise fall
        // through to the non-private (tenant/shared) class.
        let sharing = if preserve_sharing
            && self
                .repo
                .find_for_write(
                    &AccessScope::allow_all(),
                    tenant,
                    owner,
                    key,
                    SharingMode::Private,
                )
                .await?
                .is_some()
        {
            SharingMode::Private
        } else {
            sharing
        };

        // Prefetch the target row of this sharing class (own-tenant, keyed by
        // tenant+owner+key+sharing) with `allow_all`; the single PDP evaluation
        // runs once the secret's concrete type is known.
        if let Some(existing) = self
            .repo
            .find_for_write(&AccessScope::allow_all(), tenant, owner, key, sharing)
            .await?
        {
            let resolved = self.resolve_stored(existing.secret_type_uuid).await?;
            // Authorize on the concrete type BEFORE any 4xx that would reveal
            // the row exists.
            let scope = self
                .scope_for_timed(
                    ctx,
                    &authz::secret_type_resource(&resolved.gts_id),
                    actions::WRITE,
                )
                .await?;
            if !self.repo.scope_includes_tenant(&scope, tenant.0).await? {
                self.metrics.cross_tenant_denied();
                return Err(DomainError::AccessDenied { cause: None });
            }

            if create_only {
                return Err(DomainError::Conflict);
            }
            let effective_sharing = if preserve_sharing {
                existing.sharing
            } else {
                sharing
            };
            if (existing.sharing == SharingMode::Private)
                != (effective_sharing == SharingMode::Private)
            {
                return Err(DomainError::UnsupportedTransition {
                    detail: "cannot move between private and tenant/shared".into(),
                });
            }
            if let Some(requested) = opts.secret_type.as_ref()
                && requested.to_uuid() != existing.secret_type_uuid
            {
                return Err(DomainError::TypeViolation {
                    field: "type",
                    reason: typing::reasons::TYPE_IMMUTABLE,
                    detail: format!(
                        "secret is of type '{}'; changing it to '{}' is not supported",
                        resolved.gts_id, requested
                    ),
                });
            }
            typing::validate_write(
                &resolved.gts_id,
                &resolved.traits,
                effective_sharing,
                &value,
                opts.expires_at.requested(),
            )?;
            let expected_version = Self::precheck_version(precondition.as_ref(), &existing)?;
            return self
                .overwrite_existing(
                    ctx,
                    &plugin,
                    tenant,
                    &scope,
                    existing.id,
                    effective_sharing,
                    expected_version,
                    opts.expires_at.resolve(existing.expires_at),
                    value,
                )
                .await;
        }

        // The target does not exist. An update never creates.
        if !create_only {
            return Err(DomainError::VersionConflict);
        }

        // Create path: the type defaults to generic; traits + per-type access
        // validated before any side effect.
        let type_uuid = opts
            .secret_type
            .map_or_else(|| SecretType::generic().uuid(), |id| id.to_uuid());
        let resolved = self.types.resolve(type_uuid).await?;
        typing::validate_write(
            &resolved.gts_id,
            &resolved.traits,
            sharing,
            &value,
            opts.expires_at.requested(),
        )?;
        let scope = self
            .scope_for_timed(
                ctx,
                &authz::secret_type_resource(&resolved.gts_id),
                actions::WRITE,
            )
            .await?;
        if !self.repo.scope_includes_tenant(&scope, tenant.0).await? {
            self.metrics.cross_tenant_denied();
            return Err(DomainError::AccessDenied { cause: None });
        }

        self.create_new(
            ctx,
            &plugin,
            tenant,
            owner,
            key,
            sharing,
            type_uuid,
            opts.expires_at.resolve(None),
            value,
            &scope,
        )
        .await
    }

    /// Optimistic-concurrency pre-check before any backend write: returns the
    /// `version = ?` filter to gate on, or `VersionConflict` if the
    /// caller's `If-Match` validator disagrees with the current row.
    fn precheck_version(
        precondition: Option<&WritePrecondition>,
        existing: &SecretRow,
    ) -> Result<Option<i64>, DomainError> {
        match precondition {
            Some(WritePrecondition::Version { id, version }) => {
                if *id != existing.id || *version != existing.version {
                    return Err(DomainError::VersionConflict);
                }
                Ok(Some(*version))
            }
            Some(WritePrecondition::AnyVersion(validators)) => {
                if validators
                    .iter()
                    .any(|(id, version)| *id == existing.id && *version == existing.version)
                {
                    Ok(Some(existing.version))
                } else {
                    Err(DomainError::VersionConflict)
                }
            }
            Some(WritePrecondition::Exists) | None => Ok(None),
        }
    }

    /// Create-path write protocol (ADR-0006 §6.2 steps 2-4, `insert_active`
    /// variant): mint a fresh `value_id`, record its intent, write the
    /// backend, then insert the row `active` pointing at it. A create-only
    /// uniqueness conflict on step 4 aborts the just-written version
    /// (best-effort) and returns `Conflict`.
    #[allow(
        clippy::too_many_arguments,
        reason = "carries every field a create needs: identity, sharing, type, expiry, value, scope"
    )]
    async fn create_new(
        &self,
        ctx: &SecurityContext,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        tenant: TenantId,
        owner: OwnerId,
        key: &SecretRef,
        sharing: SharingMode,
        secret_type_uuid: Uuid,
        expires_at: Option<OffsetDateTime>,
        value: SecretValue,
        scope: &AccessScope,
    ) -> Result<(), DomainError> {
        let value_fp = self.stamp_fp(plugin, &value).await?;
        let new_id = ValueId::new_v4();
        self.repo.gc_insert_pending(new_id, tenant).await?;

        if let Err(e) = self
            .plugin_put_timed(plugin, ctx, &tenant, new_id, value)
            .await
        {
            self.abandon_pending(new_id).await;
            return Err(e);
        }

        let new = NewSecret {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            reference: key.clone(),
            sharing,
            owner_id: owner,
            secret_type_uuid,
            expires_at,
            value_id: new_id,
            value_fp,
            fp_key_id: fence::CURRENT_FENCE_KEY_ID,
        };
        match self.repo.insert_active(scope, &new).await {
            Ok(()) => Ok(()),
            Err(DomainError::Conflict) => {
                self.abort_new_value(ctx, plugin, &tenant, new_id).await;
                Err(DomainError::Conflict)
            }
            // Any other failure (e.g. DB unreachable) leaves new_id `pending`
            // in gc; the maintenance job's pending-reclaim pass resolves it
            // once it is older than `gc.pending_max_age_secs`.
            Err(e) => Err(e),
        }
    }

    /// Overwrite-path write protocol (ADR-0006 §6.2 steps 2-5, `switch_value`
    /// variant): mint a fresh `value_id`, record its intent, write the
    /// backend, then one transaction switches the row's pointer. A lost CAS
    /// (version mismatch, or the row vanished) aborts the just-written
    /// version (best-effort) and returns `VersionConflict`; a won CAS
    /// best-effort cleans up the version it just superseded.
    #[allow(
        clippy::too_many_arguments,
        reason = "carries every field an overwrite's CAS needs: identity, precondition, sharing, \
                  expiry, value"
    )]
    async fn overwrite_existing(
        &self,
        ctx: &SecurityContext,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        tenant: TenantId,
        scope: &AccessScope,
        existing_id: Uuid,
        sharing: SharingMode,
        expected_version: Option<i64>,
        expires_at: Option<OffsetDateTime>,
        value: SecretValue,
    ) -> Result<(), DomainError> {
        let value_fp = self.stamp_fp(plugin, &value).await?;
        let new_id = ValueId::new_v4();
        self.repo.gc_insert_pending(new_id, tenant).await?;

        if let Err(e) = self
            .plugin_put_timed(plugin, ctx, &tenant, new_id, value)
            .await
        {
            self.abandon_pending(new_id).await;
            return Err(e);
        }

        let switched = self
            .repo
            .switch_value(
                scope,
                existing_id,
                expected_version,
                sharing,
                expires_at,
                new_id,
                value_fp,
                fence::CURRENT_FENCE_KEY_ID,
            )
            .await?;

        let Some((_row, old_value_id)) = switched else {
            // Lost the CAS: version mismatch, or the row vanished
            // concurrently (delete won). The just-written backend bytes are
            // unreferenced; abort them best-effort.
            self.abort_new_value(ctx, plugin, &tenant, new_id).await;
            return Err(DomainError::VersionConflict);
        };

        if let Some(old_id) = old_value_id {
            self.cleanup_replaced_value(ctx, plugin, &tenant, old_id)
                .await;
        }
        Ok(())
    }

    /// Timed `plugin.put`, mapping the error and recording the dependency
    /// metric.
    async fn plugin_put_timed(
        &self,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        ctx: &SecurityContext,
        tenant: &TenantId,
        value_id: ValueId,
        value: SecretValue,
    ) -> Result<(), DomainError> {
        let t0 = Instant::now();
        let result = plugin
            .put(ctx, tenant, &value_id, value)
            .await
            .map_err(map_plugin_err);
        let secs = t0.elapsed().as_secs_f64();
        self.metrics.dependency(
            Dep::Plugin,
            DepOp::PluginPut,
            if result.is_ok() {
                Outcome::Success
            } else {
                Outcome::Error
            },
            secs,
        );
        result
    }

    /// Timed best-effort `plugin.delete`. `NotFound` counts as success
    /// (idempotent delete).
    async fn plugin_delete_timed(
        &self,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        ctx: &SecurityContext,
        tenant: &TenantId,
        value_id: ValueId,
    ) -> Result<(), DomainError> {
        let t0 = Instant::now();
        let result = plugin.delete(ctx, tenant, &value_id).await;
        let secs = t0.elapsed().as_secs_f64();
        let result = match result {
            Ok(()) | Err(CredStoreError::NotFound) => Ok(()),
            Err(e) => Err(map_plugin_err(e)),
        };
        self.metrics.dependency(
            Dep::Plugin,
            DepOp::PluginDelete,
            if result.is_ok() {
                Outcome::Success
            } else {
                Outcome::Error
            },
            secs,
        );
        result
    }

    /// Step-3-failure cleanup: `plugin.put` never landed, so only the
    /// intent row needs dropping. Best-effort; a failure here is simply
    /// reclaimed later by the maintenance job's pending-reclaim pass.
    async fn abandon_pending(&self, new_id: ValueId) {
        if let Err(e) = self.repo.gc_delete(new_id).await {
            Self::warn_gc_step_failed(new_id, "abandon_pending", &e);
        }
    }

    /// Log one best-effort gc-cleanup step's failure. Every such failure is
    /// benign by design — the gc row (or the version it names) simply stays
    /// for the maintenance job's next run — so this only warns, never
    /// propagates.
    fn warn_gc_step_failed(value_id: ValueId, step: &str, err: &DomainError) {
        tracing::warn!(
            value_id = %value_id,
            step,
            err = %err,
            "credstore: best-effort gc cleanup step failed; drained later by the maintenance job"
        );
    }

    /// CAS-lost cleanup (best-effort): mark the orphaned version `aborted`,
    /// then try to delete its backend entry and drop the gc row. Any step
    /// failing leaves the gc entry for the maintenance job's drain.
    async fn abort_new_value(
        &self,
        ctx: &SecurityContext,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        tenant: &TenantId,
        new_id: ValueId,
    ) {
        if let Err(e) = self.repo.gc_mark(new_id, GcReason::Aborted).await {
            Self::warn_gc_step_failed(new_id, "mark_aborted", &e);
        }
        if let Err(e) = self.plugin_delete_timed(plugin, ctx, tenant, new_id).await {
            Self::warn_gc_step_failed(new_id, "backend_delete", &e);
            return;
        }
        if let Err(e) = self.repo.gc_delete(new_id).await {
            Self::warn_gc_step_failed(new_id, "gc_delete", &e);
        }
    }

    /// Ordinary-write success cleanup (best-effort, step 5 / delete's third
    /// step): delete the superseded/removed version's backend entry, then
    /// drop its gc row. A failure leaves it enqueued for the maintenance
    /// job's drain — never retried by the caller.
    async fn cleanup_replaced_value(
        &self,
        ctx: &SecurityContext,
        plugin: &Arc<dyn CredStorePluginClientV1>,
        tenant: &TenantId,
        old_id: ValueId,
    ) {
        if let Err(e) = self.plugin_delete_timed(plugin, ctx, tenant, old_id).await {
            Self::warn_gc_step_failed(old_id, "backend_delete", &e);
            return;
        }
        if let Err(e) = self.repo.gc_delete(old_id).await {
            Self::warn_gc_step_failed(old_id, "gc_delete", &e);
        }
    }

    /// Delete an owned secret.
    ///
    /// One database transaction ([`SecretRepo::delete_by_id`]) removes the
    /// row and enqueues its version for collection; the reference is free to
    /// reuse the instant that transaction commits (ADR-0006: no name
    /// retention — a successor mints its own `value_id`, so it can never
    /// collide with this delete's lagging backend cleanup). That cleanup
    /// then runs best-effort, exactly like an ordinary write's step 5.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] if no own-tenant row exists for the key.
    /// Returns [`DomainError::VersionConflict`] if the current version does not
    /// satisfy a version-validator `precondition`. The precondition is
    /// mandatory: [`WritePrecondition::Exists`] is the explicit
    /// delete-whatever-is-there form (REST `If-Match: *`).
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        precondition: WritePrecondition,
    ) -> Result<(), DomainError> {
        let tenant = TenantId(ctx.subject_tenant_id());
        let owner = OwnerId(ctx.subject_id());

        let Some(row) = self
            .repo
            .find_own(&AccessScope::allow_all(), tenant, owner, key)
            .await?
        else {
            return Err(DomainError::NotFound);
        };

        // Optimistic-concurrency pre-check, mirroring the put path: both the
        // row id (a validator minted for a recreated secret's earlier
        // generation must never match, no ABA) and the version.
        let expected_version = Self::precheck_version(Some(&precondition), &row)?;

        // Single PDP evaluation on the secret's full concrete type, gated on the
        // caller's tenant. Delete reveals reference existence within the caller's
        // own tenant — the 404-before-PDP (no row) vs 403-after-PDP (row present,
        // denied) split is an own-tenant existence signal available to any member
        // of the tenant. This is an accepted part of the module's threat model;
        // cross-tenant existence stays hidden by the anti-enumeration 404 on the
        // read path.
        let resolved = self.resolve_stored(row.secret_type_uuid).await?;
        let scope = self
            .scope_for_timed(
                ctx,
                &authz::secret_type_resource(&resolved.gts_id),
                actions::DELETE,
            )
            .await?;
        if !self.repo.scope_includes_tenant(&scope, tenant.0).await? {
            self.metrics.cross_tenant_denied();
            return Err(DomainError::AccessDenied { cause: None });
        }

        // Fail fast if no plugin is available before the row transaction —
        // symmetric with put (which resolves the plugin before touching
        // metadata). A missing plugin here would otherwise commit the row
        // delete and only then fail the (skippable) best-effort cleanup,
        // which is harmless; resolving first just keeps the two failure
        // shapes aligned across put/delete.
        let plugin = self.plugins.resolve().await?;

        // One transaction: delete the row, enqueue its version (if any) for
        // collection. 0 rows (version mismatch, or a concurrent delete/write
        // already moved the row) maps to the existing not-found/conflict
        // semantics.
        let old_value_id = match self
            .repo
            .delete_by_id(&scope, row.id, expected_version)
            .await
        {
            Ok(v) => v,
            Err(DomainError::NotFound) if expected_version.is_some() => {
                return Err(DomainError::VersionConflict);
            }
            Err(e) => return Err(e),
        };

        if let Some(old_id) = old_value_id {
            self.cleanup_replaced_value(ctx, &plugin, &tenant, old_id)
                .await;
        }
        Ok(())
    }

    /// Run one pass of the periodic maintenance job (`credstore gc`): the
    /// expired-row sweep and the gc drain/pending-reclaim, both in bounded
    /// batches until a batch yields no progress. Idempotent — running it
    /// twice in immediate succession is a no-op the second time. Errors on
    /// one entry are logged and the loop continues to the next; the run
    /// always returns its report.
    ///
    /// # Errors
    ///
    /// Returns an error only if listing a batch itself fails (a DB outage);
    /// per-entry cleanup failures are logged and swallowed.
    pub async fn run_gc(&self, ctx: &SecurityContext) -> Result<GcReport, DomainError> {
        let mut report = GcReport::default();
        self.run_gc_expired_pass(ctx, &mut report).await?;
        self.run_gc_drain_pass(ctx, &mut report).await?;
        Ok(report)
    }

    /// Pass 1: every `active` row past its `expires_at`, in bounded batches
    /// until a batch is empty.
    async fn run_gc_expired_pass(
        &self,
        ctx: &SecurityContext,
        report: &mut GcReport,
    ) -> Result<(), DomainError> {
        loop {
            let batch = self.repo.list_expired(self.gc.batch_size).await?;
            if batch.is_empty() {
                break;
            }
            let mut progressed = false;
            for row in &batch {
                match self.repo.delete_expired_row(row.id).await {
                    Ok(Some(old_id)) => {
                        if let Some(plugin) = self.gc_plugin(ctx).await {
                            self.cleanup_replaced_value(ctx, &plugin, &row.tenant_id, old_id)
                                .await;
                        }
                        report.expired_deleted += 1;
                        progressed = true;
                    }
                    Ok(None) => {
                        // Already gone (a client delete raced the job), or
                        // was already declared with no value: nothing to
                        // reconcile, but the row itself is accounted for.
                        report.expired_deleted += 1;
                        progressed = true;
                    }
                    Err(e) => {
                        tracing::warn!(id = %row.id, err = %e, "credstore gc: delete_expired_row failed");
                    }
                }
            }
            if !progressed {
                // Every row in this batch failed to delete (e.g. a
                // persistent DB issue) — stop rather than refetch and spin
                // on the same unchanged batch forever.
                break;
            }
        }
        Ok(())
    }

    /// Pass 2: the gc drain (`reason != pending`) and the pending-reclaim
    /// pass (`reason = pending` older than `pending_max_age_secs`), in
    /// bounded batches until a batch yields no progress (nothing left to
    /// drain and nothing eligible to reclaim).
    async fn run_gc_drain_pass(
        &self,
        ctx: &SecurityContext,
        report: &mut GcReport,
    ) -> Result<(), DomainError> {
        let cutoff = OffsetDateTime::now_utc()
            - time::Duration::seconds(
                i64::try_from(self.gc.pending_max_age_secs).unwrap_or(i64::MAX),
            );
        loop {
            let batch = self.repo.gc_list(self.gc.batch_size).await?;
            if batch.is_empty() {
                break;
            }
            let mut progressed = false;
            for entry in &batch {
                let entry_progressed = match entry.reason {
                    GcReason::Pending => {
                        self.drain_pending_entry(
                            ctx,
                            entry,
                            cutoff,
                            &mut report.gc_pending_reclaimed,
                        )
                        .await?
                    }
                    GcReason::Superseded | GcReason::Removed | GcReason::Aborted => {
                        self.drain_terminal_entry(
                            ctx,
                            &entry.tenant_id,
                            entry.value_id,
                            &mut report.gc_deleted,
                        )
                        .await?
                    }
                };
                progressed |= entry_progressed;
            }
            if !progressed {
                // Nothing in this batch could be advanced (e.g. every entry
                // is young pending noise) — stop rather than spin forever on
                // the same unchanged batch.
                break;
            }
        }
        Ok(())
    }

    /// One `pending` gc entry: skip if too young, drop-without-touching-the-
    /// backend if a live row still references it (the defensive branch), else
    /// drain it like any other terminal entry. Returns whether this call
    /// advanced the batch.
    async fn drain_pending_entry(
        &self,
        ctx: &SecurityContext,
        entry: &GcEntry,
        cutoff: OffsetDateTime,
        counter: &mut u64,
    ) -> Result<bool, DomainError> {
        if entry.enqueued_at > cutoff {
            // Too young: by protocol a committed pointer never leaves its
            // intent behind, so a young pending entry is normal in-flight-
            // write noise, not yet reclaimable.
            return Ok(false);
        }
        if self.repo.is_value_referenced(entry.value_id).await? {
            // Defensive branch (should be unreachable by protocol): a live
            // reference to a still-pending id. Never touch the backend; just
            // drop the stale intent row.
            return self.repo.gc_delete(entry.value_id).await;
        }
        self.drain_terminal_entry(ctx, &entry.tenant_id, entry.value_id, counter)
            .await
    }

    /// One already-decided gc entry (`superseded`/`removed`/`aborted`, or a
    /// `pending` entry that cleared the pending-only checks): best-effort
    /// backend delete, then drop the gc row and count it on success. Returns
    /// whether this call advanced the batch.
    async fn drain_terminal_entry(
        &self,
        ctx: &SecurityContext,
        tenant: &TenantId,
        value_id: ValueId,
        counter: &mut u64,
    ) -> Result<bool, DomainError> {
        let Some(plugin) = self.gc_plugin(ctx).await else {
            return Ok(false);
        };
        match self
            .plugin_delete_timed(&plugin, ctx, tenant, value_id)
            .await
        {
            Ok(()) => {
                if self.repo.gc_delete(value_id).await? {
                    *counter += 1;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(e) => {
                Self::warn_gc_step_failed(value_id, "gc_drain_delete", &e);
                Ok(false)
            }
        }
    }

    /// Resolve the plugin for the maintenance job, logging (once per pass
    /// iteration) and returning `None` if none is available — the job must
    /// survive a transient/misconfigured backend and simply make no backend
    /// progress this round.
    async fn gc_plugin(&self, _ctx: &SecurityContext) -> Option<Arc<dyn CredStorePluginClientV1>> {
        match self.plugins.resolve().await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(err = %e, "credstore gc: no plugin available this pass");
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
