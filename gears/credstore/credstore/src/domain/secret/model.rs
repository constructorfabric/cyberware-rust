//! Domain models for secret metadata and write concurrency (ADR-0006:
//! immutable value versions).
//!
//! Models the two resting statuses (`active`/`declared`), the fallback
//! policy, the version pointer, optimistic preconditions, and the
//! garbage-collection intent/work-queue entries, all persisted separately
//! from secret values.

use credstore_sdk::{OwnerId, SecretRef, SharingMode, TenantId, ValueId, WriteOptions};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// A row's lifecycle status. `CHECK (status IN (2, 4))` at the storage layer
/// admits only these two; codes `1` (`provisioning`) and `3`
/// (`deprovisioning`) are retired by ADR-0006 and never reassigned — a stray
/// reference to either in an old dashboard or log line stays unambiguous.
///
/// There is no in-flight status: a write is a value-and-pointer switch inside
/// one database transaction, not a sequence of externally observable saga
/// steps, so nothing observable precedes the commit that makes a row
/// consistent.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStatus {
    /// The row resolves and points at a value (`value_id IS NOT NULL`).
    Active,
    /// The row holds its reference but carries no value (`value_id IS
    /// NULL`), reached only via a value-removal write. Never a resolution
    /// candidate in Phase 1 (`resolve_for_get` still filters `status = 2`).
    Declared,
}
impl SecretStatus {
    #[must_use]
    pub fn as_smallint(self) -> i16 {
        match self {
            Self::Active => 2,
            Self::Declared => 4,
        }
    }

    /// Decode a stored status code. `1`/`3` (retired) and any other value are
    /// out-of-domain — a storage corruption, not a reachable application
    /// state — so this returns `None` for the caller to map onto
    /// [`crate::domain::error::DomainError::Internal`], never a panic.
    #[must_use]
    pub fn from_smallint(v: i16) -> Option<Self> {
        match v {
            2 => Some(Self::Active),
            4 => Some(Self::Declared),
            _ => None,
        }
    }
}

/// Suppression policy for a `declared` row (ADR-0004, modelled here because
/// `m0002` stores it alongside the two-status/pointer schema). Phase 1 never
/// writes `None` — no write path produces a `declared` row yet — but the
/// column and its decode/encode live in the domain now so the storage layer
/// and future write paths share one representation.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// Resolution keeps walking up the tenant chain past this row (default).
    Inherit,
    /// This row blocks resolution outright when it is the nearest candidate.
    None,
}
impl Fallback {
    #[must_use]
    pub fn as_smallint(self) -> i16 {
        match self {
            Self::Inherit => 1,
            Self::None => 2,
        }
    }

    /// Decode a stored fallback code; out-of-domain values are storage
    /// corruption, mapped by the caller onto `DomainError::Internal`.
    #[must_use]
    pub fn from_smallint(v: i16) -> Option<Self> {
        match v {
            1 => Some(Self::Inherit),
            2 => Some(Self::None),
            _ => None,
        }
    }
}

#[domain_model]
#[derive(Debug, Clone)]
pub struct SecretRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub reference: String,
    pub sharing: SharingMode,
    pub owner_id: OwnerId,
    pub status: SecretStatus,
    /// Monotonic version (optimistic-locking); 1 on create, bumped by every
    /// successful value switch or metadata update.
    pub version: i64,
    /// Deterministic v5 UUID of the secret's GTS type id (the stored
    /// representation); immutable for the row's lifetime. Resolved to the
    /// type id + traits via the types-registry per operation.
    pub secret_type_uuid: Uuid,
    /// Expiry instant for expirable types; expired rows do not resolve.
    pub expires_at: Option<OffsetDateTime>,
    /// Pointer to this row's current backend version. `None` iff `status =
    /// Declared` (`ck_credstore_fp_with_value`): a row points at a version or
    /// at nothing, never at a value with no fingerprint.
    pub value_id: Option<ValueId>,
    /// Value-fingerprint fence (`HMAC-SHA256(fence_key, value)`) of the
    /// backend value `value_id` names. `Some` iff `value_id` is — a
    /// fingerprint exists for every value and a value-less row has none;
    /// out-of-band seeding (a value with no fingerprint) is withdrawn by
    /// ADR-0006. Internal-only: never serialized to any API response or log.
    pub value_fp: Option<Vec<u8>>,
    /// Fence-key id `value_fp` was computed under; `Some` iff `value_fp` is.
    pub fp_key_id: Option<i16>,
    /// Suppression policy; only consulted for a `Declared` row's fallback
    /// competition (ADR-0004). Always `Inherit` in Phase 1.
    pub fallback: Fallback,
}

/// Everything a write needs beyond identity and value: sharing mode,
/// create-only vs update, optimistic-concurrency precondition, and typed
/// options (secret type + expiry).
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct WriteSpec {
    pub sharing: SharingMode,
    pub create_only: bool,
    /// Mandatory for updates (`Some` from [`Self::update`]), absent only on
    /// the create path (`None` from [`Self::create`]) — creates are the one
    /// preconditionless write.
    pub precondition: Option<WritePrecondition>,
    pub opts: WriteOptions,
    /// When overwriting an existing secret, keep the stored sharing mode
    /// instead of applying `sharing`. Set for a PUT that omitted `sharing`, so
    /// a value rotation (`{"value": "..."}`) never silently narrows a `shared`
    /// secret back to `tenant`. `sharing` is still the class selector.
    pub preserve_sharing: bool,
}

impl WriteSpec {
    /// Update of an existing secret with default options. The
    /// optimistic-concurrency precondition is mandatory: a version validator
    /// for read-modify-write, [`WritePrecondition::Exists`] for an explicit
    /// last-writer-wins overwrite. An update never creates.
    #[must_use]
    pub fn update(sharing: SharingMode, precondition: WritePrecondition) -> Self {
        Self {
            sharing,
            precondition: Some(precondition),
            ..Self::default()
        }
    }

    /// Create-only (409 on same-class duplicate) with default options.
    #[must_use]
    pub fn create(sharing: SharingMode) -> Self {
        Self {
            sharing,
            create_only: true,
            ..Self::default()
        }
    }

    /// Attach typed write options (secret type, expiry).
    #[must_use]
    pub fn with_opts(mut self, opts: WriteOptions) -> Self {
        self.opts = opts;
        self
    }

    /// Preserve the existing secret's sharing on overwrite (see field docs).
    #[must_use]
    pub fn preserve_sharing(mut self, preserve: bool) -> Self {
        self.preserve_sharing = preserve;
        self
    }
}

/// Optimistic-concurrency precondition for a write, parsed from `If-Match`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritePrecondition {
    /// `If-Match: *` — the target secret must already exist.
    Exists,
    /// `If-Match: "<id>.<version>"` — the generation-bound strong validator.
    /// `id` is the row UUID (a fresh one per recreated secret), so a
    /// validator from a deleted-and-recreated secret's earlier generation can
    /// never match the current row even when the version counters coincide
    /// (no ABA); `version` is the per-row monotonic counter.
    Version {
        /// Row (generation) UUID the caller's validator was minted for.
        id: Uuid,
        /// Version counter the caller last observed.
        version: i64,
    },
    /// `If-Match: "<id>.<v>", "<id2>.<v2>", …` — a multi-valued list (RFC 7232
    /// §3.1). The precondition is satisfied if the current row matches **any**
    /// listed `(id, version)` validator.
    AnyVersion(Vec<(Uuid, i64)>),
}

/// A new active row: a create always inserts `active`, pointing at the
/// version its `plugin.put` already wrote.
#[domain_model]
#[derive(Debug, Clone)]
pub struct NewSecret {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub reference: SecretRef,
    pub sharing: SharingMode,
    pub owner_id: OwnerId,
    /// Deterministic v5 UUID of the (registry-validated) GTS type id.
    pub secret_type_uuid: Uuid,
    pub expires_at: Option<OffsetDateTime>,
    /// The fresh backend version this create's value was written to.
    pub value_id: ValueId,
    /// Fence fingerprint of the value this create wrote to the backend.
    pub value_fp: Vec<u8>,
    /// Fence-key id `value_fp` was computed under.
    pub fp_key_id: i16,
}

/// Why a `credstore_value_gc` entry was enqueued (`reason` column,
/// `CHECK (reason IN (1, 2, 3, 4))`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcReason {
    /// A write's intent, recorded before the backend call — the durable
    /// trace that lets the maintenance job reconcile a crash the plugin
    /// cannot be asked to list its way out of.
    Pending,
    /// Replaced by a newer version at the same row (an ordinary overwrite).
    Superseded,
    /// The record (or its value) was removed — `DELETE` or a value-removal
    /// write.
    Removed,
    /// The write that produced this version lost its CAS; the version was
    /// written but no row ever points at it.
    Aborted,
}
impl GcReason {
    #[must_use]
    pub fn as_smallint(self) -> i16 {
        match self {
            Self::Pending => 1,
            Self::Superseded => 2,
            Self::Removed => 3,
            Self::Aborted => 4,
        }
    }

    /// Decode a stored reason code; out-of-domain values are storage
    /// corruption, mapped by the caller onto `DomainError::Internal`.
    #[must_use]
    pub fn from_smallint(v: i16) -> Option<Self> {
        match v {
            1 => Some(Self::Pending),
            2 => Some(Self::Superseded),
            3 => Some(Self::Removed),
            4 => Some(Self::Aborted),
            _ => None,
        }
    }
}

/// One `credstore_value_gc` row: a version the maintenance job's gc drain (or
/// pending-reclaim pass) must eventually resolve — reconcile a crashed write,
/// or delete a backend entry nothing points to any more.
#[domain_model]
#[derive(Debug, Clone)]
pub struct GcEntry {
    pub value_id: ValueId,
    pub tenant_id: TenantId,
    pub reason: GcReason,
    pub enqueued_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::{Fallback, GcReason, SecretStatus};

    #[test]
    fn secret_status_smallint_round_trips() {
        for s in [SecretStatus::Active, SecretStatus::Declared] {
            assert_eq!(SecretStatus::from_smallint(s.as_smallint()), Some(s));
        }
        assert_eq!(SecretStatus::Active.as_smallint(), 2);
        assert_eq!(SecretStatus::Declared.as_smallint(), 4);
    }

    #[test]
    fn secret_status_from_smallint_rejects_retired_and_out_of_domain_codes() {
        // 1 (provisioning) and 3 (deprovisioning) are retired by ADR-0006:
        // reserved, never reassigned, never stored again.
        assert_eq!(SecretStatus::from_smallint(1), None);
        assert_eq!(SecretStatus::from_smallint(3), None);
        assert_eq!(SecretStatus::from_smallint(0), None);
        assert_eq!(SecretStatus::from_smallint(5), None);
        assert_eq!(SecretStatus::from_smallint(-1), None);
    }

    #[test]
    fn fallback_smallint_round_trips() {
        for f in [Fallback::Inherit, Fallback::None] {
            assert_eq!(Fallback::from_smallint(f.as_smallint()), Some(f));
        }
        assert_eq!(Fallback::Inherit.as_smallint(), 1);
        assert_eq!(Fallback::None.as_smallint(), 2);
    }

    #[test]
    fn fallback_from_smallint_rejects_out_of_domain() {
        assert_eq!(Fallback::from_smallint(0), None);
        assert_eq!(Fallback::from_smallint(3), None);
    }

    #[test]
    fn gc_reason_smallint_round_trips() {
        for r in [
            GcReason::Pending,
            GcReason::Superseded,
            GcReason::Removed,
            GcReason::Aborted,
        ] {
            assert_eq!(GcReason::from_smallint(r.as_smallint()), Some(r));
        }
        assert_eq!(GcReason::Pending.as_smallint(), 1);
        assert_eq!(GcReason::Superseded.as_smallint(), 2);
        assert_eq!(GcReason::Removed.as_smallint(), 3);
        assert_eq!(GcReason::Aborted.as_smallint(), 4);
    }

    #[test]
    fn gc_reason_from_smallint_rejects_out_of_domain() {
        assert_eq!(GcReason::from_smallint(0), None);
        assert_eq!(GcReason::from_smallint(5), None);
    }
}
