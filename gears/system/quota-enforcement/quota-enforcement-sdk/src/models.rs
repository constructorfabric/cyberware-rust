//! Domain types referenced by the quota-enforcement plugin contracts.
//!
//! Type-stability rules (DESIGN section 3.1):
//!
//! - Every enum is closed at the SDK boundary. Deserialization rejects an
//!   unknown value instead of a fallback variant.
//! - GTS-anchored enums (`QuotaType`, `EnforcementMode`, `QuotaSource`,
//!   `PeriodType`) serialize as their full GTS instance id. Storage rows and
//!   events carry that form.
//! - Timestamps serialize as RFC 3339 in UTC.
//! - Input shapes (`QuotaDraft`, `QuotaPatch`, `PolicyDraft`, `PolicyUpdate`)
//!   reject unknown fields.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use gts::{GtsIdError, GtsInstanceId, GtsTypeId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::serde::rfc3339;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Wrap an existing identifier.
            #[must_use]
            pub const fn new(id: Uuid) -> Self {
                Self(id)
            }

            /// Mint a fresh, time-ordered identifier (`UUIDv7`).
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// The raw UUID.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

uuid_id!(
    /// Server-assigned Quota identifier. `UUIDv7`, so that lexicographic order is
    /// acquisition order (ADR-0002).
    QuotaId
);
uuid_id!(
    /// Tenant identifier. PDP-authorized before it reaches storage.
    TenantId
);
uuid_id!(
    /// Opaque two-phase lease token.
    LeaseToken
);
uuid_id!(
    /// Identifier of one consumption-period counter row.
    PeriodId
);
uuid_id!(
    /// Notification outbox event identifier.
    EventId
);

/// Stable identifier of a Quota Resolution Policy. The seeded platform policy
/// is [`PolicyId::GLOBAL`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PolicyId(String);

impl PolicyId {
    /// Identifier of the seeded `global` policy. It cannot be deleted.
    pub const GLOBAL: &'static str = "global";

    /// Wrap an identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The seeded global policy identifier.
    #[must_use]
    pub fn global() -> Self {
        Self(Self::GLOBAL.to_owned())
    }

    /// True for the seeded global policy.
    #[must_use]
    pub fn is_global(&self) -> bool {
        self.0 == Self::GLOBAL
    }

    /// The raw identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PolicyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Registered metric identity: a `types-registry` instance id.
///
/// QE mints no metric names. The value is validated against the registry at
/// Quota create and update time, never on the evaluation path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MetricId(GtsInstanceId);

impl MetricId {
    /// Wrap an already validated instance id.
    #[must_use]
    pub const fn new(id: GtsInstanceId) -> Self {
        Self(id)
    }

    /// Parse a full GTS instance id.
    ///
    /// # Errors
    ///
    /// Returns the GTS parse error when `raw` is not a well-formed instance id.
    pub fn parse(raw: &str) -> Result<Self, GtsIdError> {
        GtsInstanceId::try_new(raw).map(Self)
    }

    /// The underlying instance id.
    #[must_use]
    pub const fn as_gts(&self) -> &GtsInstanceId {
        &self.0
    }

    /// The canonical string form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl fmt::Display for MetricId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Digests
// ---------------------------------------------------------------------------

macro_rules! digest_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 32]);

        impl $name {
            /// Wrap a SHA-256 digest.
            #[must_use]
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            /// The raw digest bytes.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// Lowercase hexadecimal form.
            #[must_use]
            pub fn to_hex(self) -> String {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let mut out = String::with_capacity(64);
                for byte in self.0 {
                    out.push(char::from(HEX[usize::from(byte >> 4)]));
                    out.push(char::from(HEX[usize::from(byte & 0x0f)]));
                }
                out
            }

            /// Parse the lowercase or uppercase hexadecimal form.
            ///
            /// # Errors
            ///
            /// Returns [`UnknownValue`] when `hex` is not 64 hexadecimal digits.
            pub fn parse_hex(hex: &str) -> Result<Self, UnknownValue> {
                let bad = || UnknownValue {
                    kind: stringify!($name),
                    value: hex.to_owned(),
                };
                if hex.len() != 64 {
                    return Err(bad());
                }
                let mut bytes = [0_u8; 32];
                for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
                    let pair = std::str::from_utf8(chunk).map_err(|_| bad())?;
                    bytes[i] = u8::from_str_radix(pair, 16).map_err(|_| bad())?;
                }
                Ok(Self(bytes))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_hex())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw: Cow<'de, str> = Deserialize::deserialize(d)?;
                Self::parse_hex(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

digest_newtype!(
    /// Fixed-width fingerprint of the canonical applicable-subject set:
    /// sort and deduplicate `(projection_type, subject_id)` pairs, encode them
    /// canonically, hash with SHA-256 (PRD section 5.8).
    IdempotencySubjectKey
);
digest_newtype!(
    /// SHA-256 of the canonical sorted-JSON request payload.
    PayloadHash
);

/// An unknown value was supplied for a closed enum or a fixed-width digest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} value: {value}")]
pub struct UnknownValue {
    /// Which closed set rejected the value.
    pub kind: &'static str,
    /// The rejected value.
    pub value: String,
}

// ---------------------------------------------------------------------------
// GTS-anchored closed enums
// ---------------------------------------------------------------------------

macro_rules! gts_closed_enum {
    (
        $(#[$meta:meta])*
        $name:ident, kind = $kind:literal, base = $base:literal, {
            $( $(#[$vmeta:meta])* $variant:ident => $id:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// Abstract GTS base type every value derives from.
            pub const BASE_TYPE_ID: &'static str = $base;

            /// Every value, in declaration order.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            /// Full GTS instance id of this value: the wire and storage form.
            #[must_use]
            pub const fn as_gts_id(self) -> &'static str {
                match self {
                    $( Self::$variant => $id, )+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_gts_id())
            }
        }

        impl FromStr for $name {
            type Err = UnknownValue;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $id => Ok(Self::$variant), )+
                    other => Err(UnknownValue { kind: $kind, value: other.to_owned() }),
                }
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_gts_id())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw: Cow<'de, str> = Deserialize::deserialize(d)?;
                raw.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

gts_closed_enum!(
    /// Accounting model of a Quota (PRD section 5.3).
    QuotaType, kind = "quota_type", base = "gts.cf.qe.quota.type.v1~", {
        /// In-flight reservable capacity, no period reset.
        Allocation => "gts.cf.qe.quota.type.v1~cf.qe.quota.allocation.v1",
        /// Per-period cumulative consumption, reset at the period boundary.
        Consumption => "gts.cf.qe.quota.type.v1~cf.qe.quota.consumption.v1",
        /// Reserved for P3. Creation is rejected in P1.
        Rate => "gts.cf.qe.quota.type.v1~cf.qe.quota.rate.v1",
    }
);

gts_closed_enum!(
    /// Behaviour of a Quota at its cap boundary (PRD section 5.11).
    EnforcementMode, kind = "enforcement_mode", base = "gts.cf.qe.enforcement.type.v1~", {
        /// Operations that would cross the cap are denied. The only P1 mode.
        Hard => "gts.cf.qe.enforcement.type.v1~cf.qe.enforcement.hard.v1",
    }
);

gts_closed_enum!(
    /// Who imposed the Quota (PRD section 5.2, "Source value semantics").
    QuotaSource, kind = "quota_source", base = "gts.cf.qe.source.type.v1~", {
        /// Materialized from the licensing layer. The default.
        Licensing => "gts.cf.qe.source.type.v1~cf.qe.source.licensing.v1",
        /// Created manually by an operator outside the licensing flow.
        Operator => "gts.cf.qe.source.type.v1~cf.qe.source.operator.v1",
    }
);

gts_closed_enum!(
    /// Calendar-aligned UTC period of a consumption Quota (PRD section 5.4).
    PeriodType, kind = "period_type", base = "gts.cf.qe.period.type.v1~", {
        /// 00:00 UTC to 24:00 UTC.
        Day => "gts.cf.qe.period.type.v1~cf.qe.period.day.v1",
        /// Monday 00:00 UTC to the next Monday.
        Week => "gts.cf.qe.period.type.v1~cf.qe.period.week.v1",
        /// First day of the month, 00:00 UTC.
        Month => "gts.cf.qe.period.type.v1~cf.qe.period.month.v1",
        /// First of January, 00:00 UTC.
        Year => "gts.cf.qe.period.type.v1~cf.qe.period.year.v1",
        /// Non-recurring. No automatic reset.
        OneTime => "gts.cf.qe.period.type.v1~cf.qe.period.one_time.v1",
    }
);

// ---------------------------------------------------------------------------
// Plain closed enums
// ---------------------------------------------------------------------------

/// Lifecycle state of a Quota record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    /// Accepts debits and leases.
    Active,
    /// Retained for reads. Accepts no new debits or leases.
    Deactivated,
}

/// Lease state machine. Every state except `Active` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Capacity is held.
    Active,
    /// Converted into a debit.
    Committed,
    /// Held capacity returned by the holder.
    Released,
    /// TTL elapsed without commit or release.
    AutoReleased,
    /// Resolved atomically with the deactivation of a held Quota.
    ResolvedByDeactivation,
}

impl LeaseState {
    /// True when no further transition is possible.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Active)
    }
}

/// State of one immutable Policy version (PRD section 5.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVersionState {
    /// The version the latest pointer names. At most one per policy.
    Active,
    /// Replaced by a later active version.
    Superseded,
    /// Abandoned through rollback. Terminal.
    RolledBack,
    /// The previously active version of a soft-deleted policy. Terminal.
    Deleted,
}

/// Write operation kinds that carry an idempotency key (PRD section 5.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationType {
    /// Single-shot debit.
    Debit,
    /// Compensating credit against one named Quota.
    Credit,
    /// Reversal of a prior debit by its original key.
    Rollback,
    /// Lease acquisition.
    Reserve,
    /// Lease commit.
    Commit,
    /// Lease release.
    Release,
    /// Batch debit envelope.
    BatchDebit,
}

impl OperationType {
    /// Stable `snake_case` name, also used as the storage discriminator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debit => "debit",
            Self::Credit => "credit",
            Self::Rollback => "rollback",
            Self::Reserve => "reserve",
            Self::Commit => "commit",
            Self::Release => "release",
            Self::BatchDebit => "batch_debit",
        }
    }
}

/// Closed notification event catalog (PRD section 5.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationEventKind {
    /// Consumed amount crossed at least one configured threshold upward.
    ThresholdCrossed,
    /// A consumption Quota crossed a period boundary and settled.
    PeriodRollover,
    /// A lease TTL expired without commit or release.
    LeaseAutoReleased,
    /// A lease was resolved when its Quota was deactivated.
    LeaseResolvedByDeactivation,
    /// A Quota was created, updated, or deactivated.
    QuotaChanged,
    /// A credit was applied outside the debit and rollback flow.
    QuotaCounterAdjusted,
    /// A committed debit was reversed.
    QuotaRollbackApplied,
    /// A Policy was created, updated, rolled back, or deleted.
    PolicyChanged,
}

// ---------------------------------------------------------------------------
// Subjects and contracts
// ---------------------------------------------------------------------------

/// Storage-facing subject identity after catalogue mapping.
///
/// The projection type never arrives on the wire. The gear maps the caller's
/// `(metric, kind)` through its validated catalogue after PDP authorization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubjectRef {
    /// Concrete owner projection derived from `gts.cf.core.qe.subj.v1~`.
    pub projection_type: GtsTypeId,
    /// Opaque, non-empty subject identifier.
    pub subject_id: String,
}

/// The constraint contract a Quota's metadata was validated against.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContractRef {
    /// Concrete contract type derived from `gts.cf.core.qe.constraint.v1~`.
    pub type_id: GtsTypeId,
    /// Accepted contract version.
    pub version: u32,
}

/// Optional validity bounds of a Quota. Both ends are inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidityWindow {
    /// Start of validity, inclusive. `None` means no lower bound.
    #[serde(default, with = "rfc3339::option")]
    pub start: Option<OffsetDateTime>,
    /// End of validity, inclusive. `None` means no upper bound.
    #[serde(default, with = "rfc3339::option")]
    pub end: Option<OffsetDateTime>,
}

impl ValidityWindow {
    /// True when `at` lies within the window.
    #[must_use]
    pub fn contains(&self, at: OffsetDateTime) -> bool {
        self.start.is_none_or(|s| s <= at) && self.end.is_none_or(|e| at <= e)
    }
}

// ---------------------------------------------------------------------------
// Quota
// ---------------------------------------------------------------------------

/// A stored Quota record (DESIGN section 3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "`quota_type` is the PRD field name; `type` is a keyword and `kind` would drift from the wire contract"
)]
pub struct Quota {
    /// Server-assigned identifier.
    pub id: QuotaId,
    /// PDP-authorized owning tenant.
    pub tenant_id: TenantId,
    /// Subject the Quota is bound to. Immutable after creation.
    pub subject: SubjectRef,
    /// Registered metric. Immutable after creation.
    pub metric: MetricId,
    /// Accounting model. Immutable after creation.
    pub quota_type: QuotaType,
    /// Period specification. Present for consumption Quotas only.
    pub period: Option<PeriodType>,
    /// Behaviour at the cap boundary.
    pub enforcement_mode: EnforcementMode,
    /// Cap in metric units. `None` means unbounded.
    pub cap: Option<u64>,
    /// Notification thresholds as percentages of cap, ascending.
    pub notification_thresholds: Vec<u8>,
    /// Optional validity bounds.
    pub validity_window: Option<ValidityWindow>,
    /// Informational hint for callers: prefer fail-open when QE is unavailable.
    pub fail_open_hint: bool,
    /// Operator-authored, contract-validated attributes. Opaque to QE core.
    pub metadata: Map<String, Value>,
    /// Who imposed the Quota.
    pub source: QuotaSource,
    /// Lifecycle state.
    pub status: QuotaStatus,
    /// Contract the metadata was validated against.
    pub constraint_contract: ContractRef,
    /// Optimistic-concurrency record version.
    pub record_version: u32,
    /// Creation time.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last mutation time.
    #[serde(with = "rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Create input for a Quota. The gear validates it before storage sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaDraft {
    /// PDP-authorized target tenant.
    pub tenant_id: TenantId,
    /// Catalogue-mapped subject.
    pub subject: SubjectRef,
    /// Registered metric.
    pub metric: MetricId,
    /// Accounting model.
    pub quota_type: QuotaType,
    /// Period specification. Consumption Quotas only.
    #[serde(default)]
    pub period: Option<PeriodType>,
    /// Behaviour at the cap boundary.
    pub enforcement_mode: EnforcementMode,
    /// Cap in metric units. `None` means unbounded.
    #[serde(default)]
    pub cap: Option<u64>,
    /// Notification thresholds as percentages of cap.
    #[serde(default)]
    pub notification_thresholds: Vec<u8>,
    /// Optional validity bounds.
    #[serde(default)]
    pub validity_window: Option<ValidityWindow>,
    /// Informational fail-open hint. Defaults to fail-closed.
    #[serde(default)]
    pub fail_open_hint: bool,
    /// Contract-validated metadata.
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// Who imposes the Quota.
    pub source: QuotaSource,
    /// Contract the metadata was validated against.
    pub constraint_contract: ContractRef,
}

/// Patch of a Quota's `cap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapPatch {
    /// Set a numeric cap.
    Bounded(u64),
    /// Remove the cap.
    Unbounded,
}

/// Patch of a Quota's `validity_window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidityWindowPatch {
    /// Remove the window.
    Clear,
    /// Replace the window.
    Set(ValidityWindow),
}

/// Partial update of a Quota. Absent fields stay untouched. Metric, type,
/// period, and subject are immutable and therefore absent here.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaPatch {
    /// New cap.
    pub cap: Option<CapPatch>,
    /// New thresholds, replacing the previous list.
    pub notification_thresholds: Option<Vec<u8>>,
    /// New validity window.
    pub validity_window: Option<ValidityWindowPatch>,
    /// New metadata object, replacing the previous one.
    pub metadata: Option<Map<String, Value>>,
    /// New enforcement mode.
    pub enforcement_mode: Option<EnforcementMode>,
    /// New fail-open hint.
    pub fail_open_hint: Option<bool>,
}

impl QuotaPatch {
    /// True when the patch changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Filter for [`crate::QuotaEnforcementStoragePluginV1::read_quotas`].
/// Every set field narrows the result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaFilter {
    /// Owning tenant.
    pub tenant_id: Option<TenantId>,
    /// Bound subject.
    pub subject: Option<SubjectRef>,
    /// Metric.
    pub metric: Option<MetricId>,
    /// Lifecycle state.
    pub status: Option<QuotaStatus>,
    /// Explicit identifiers. Empty means no restriction.
    pub ids: Vec<QuotaId>,
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

/// Cursor-based page request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRequest {
    /// Maximum number of items. Storage may clamp it to its own maximum.
    pub limit: u32,
    /// Opaque continuation cursor from a previous [`PageResult`].
    #[serde(default)]
    pub cursor: Option<String>,
}

impl PageRequest {
    /// Platform default page size.
    pub const DEFAULT_LIMIT: u32 = 100;

    /// First page with the given limit.
    #[must_use]
    pub const fn first(limit: u32) -> Self {
        Self {
            limit,
            cursor: None,
        }
    }
}

impl Default for PageRequest {
    fn default() -> Self {
        Self::first(Self::DEFAULT_LIMIT)
    }
}

/// One page of results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageResult<T> {
    /// Items of this page.
    pub items: Vec<T>,
    /// Cursor for the next page. `None` on the last page.
    pub next_cursor: Option<String>,
}

impl<T> PageResult<T> {
    /// An empty final page.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            items: Vec::new(),
            next_cursor: None,
        }
    }

    /// Map every item, keeping the cursor.
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> PageResult<U> {
        PageResult {
            items: self.items.into_iter().map(f).collect(),
            next_cursor: self.next_cursor,
        }
    }
}

impl<T> Default for PageResult<T> {
    fn default() -> Self {
        Self::empty()
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// The PDP-authorized, catalogue-mapped subject set of one operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicableQuotas {
    /// Authorized target tenant.
    pub tenant_id: TenantId,
    /// Every applicable subject, tenant scope included.
    pub subjects: Vec<SubjectRef>,
    /// The operation's metric.
    pub metric: MetricId,
}

/// Per-Quota mutation directive inside a [`Decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaDebitPlan {
    /// Total counter mutation for the Quota. Never negative.
    pub amount: u64,
}

/// Which Quotas to mutate and by how much.
pub type DebitPlan = BTreeMap<QuotaId, QuotaDebitPlan>;

/// Two-arm verdict of an evaluation (PRD section 3.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DecisionResult {
    /// The operation is within every applicable cap.
    Allowed,
    /// At least one Quota would be exceeded. Counters are unchanged.
    Denied {
        /// Every violating Quota. Empty when no Quota applied at all.
        violated_quota_ids: Vec<QuotaId>,
        /// Closed reason token, for example `NO_APPLICABLE_QUOTA`.
        reason: String,
    },
}

/// Engine output. Server-derived, response-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// The verdict.
    pub result: DecisionResult,
    /// Mutation plan. Empty when the result is `Denied`.
    #[serde(default)]
    pub debit_plan: DebitPlan,
    /// Engine-supplied per-Quota detail.
    #[serde(default)]
    pub diagnostics: BTreeMap<String, Value>,
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

/// Full idempotency scope: `(tenant_id, subject_key, operation_type, key)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyScope {
    /// Authorized target tenant.
    pub tenant_id: TenantId,
    /// Fingerprint of the canonical subject set.
    pub subject_key: IdempotencySubjectKey,
    /// Operation kind.
    pub operation_type: OperationType,
    /// Client-supplied key.
    pub key: String,
}

/// What a mutating primitive persists as the idempotency record, in the same
/// transaction as the mutation (invariants I1 and I2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyWrite {
    /// Full scope.
    pub scope: IdempotencyScope,
    /// Canonical payload digest for replay comparison.
    pub payload_hash: PayloadHash,
    /// The outcome to replay verbatim.
    pub decision: Decision,
    /// Engine that produced the decision.
    pub engine_id: String,
    /// Policy that produced the decision.
    pub policy_id: PolicyId,
    /// Policy version that produced the decision.
    pub policy_version: u32,
}

/// A stored idempotency record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    /// Full scope.
    pub scope: IdempotencyScope,
    /// Canonical payload digest.
    pub payload_hash: PayloadHash,
    /// Schema-versioned decision blob (top-level `__version`).
    pub decision_blob: Value,
    /// Engine that produced the decision.
    pub engine_id: String,
    /// Policy that produced the decision.
    pub policy_id: PolicyId,
    /// Policy version that produced the decision.
    pub policy_version: u32,
    /// Record creation time.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// Retention deadline.
    #[serde(with = "rfc3339")]
    pub expires_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// Events and mutation results
// ---------------------------------------------------------------------------

/// Same-transaction outbox event (invariant I11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationEvent {
    /// Unique event identifier. Sinks deduplicate on it.
    pub event_id: EventId,
    /// Catalog kind.
    pub kind: NotificationEventKind,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Target Quota, when applicable.
    #[serde(default)]
    pub quota_id: Option<QuotaId>,
    /// Target Policy, when applicable.
    #[serde(default)]
    pub policy_id: Option<PolicyId>,
    /// Subject, when applicable.
    #[serde(default)]
    pub subject: Option<SubjectRef>,
    /// Event-specific payload.
    pub payload: Value,
    /// Emission time.
    #[serde(with = "rfc3339")]
    pub emitted_at: OffsetDateTime,
}

/// Post-mutation counter value of one Quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterSnapshot {
    /// The Quota.
    pub quota_id: QuotaId,
    /// The period row for consumption Quotas.
    pub period_id: Option<PeriodId>,
    /// Consumed or in-flight amount after the mutation.
    pub value: u64,
}

/// Upward threshold transition observed by a mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThresholdCrossing {
    /// The Quota.
    pub quota_id: QuotaId,
    /// Every threshold the mutation crossed, ascending.
    pub crossed_thresholds: Vec<u8>,
    /// Maximum of `crossed_thresholds`.
    pub highest_crossed_threshold: u8,
}

/// Return value of every mutating storage primitive.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MutationResult {
    /// New counter values.
    pub counters: Vec<CounterSnapshot>,
    /// Threshold crossings the mutation produced.
    pub threshold_crossings: Vec<ThresholdCrossing>,
    /// Identifiers of the events enqueued in the same transaction.
    pub event_ids: Vec<EventId>,
}

// ---------------------------------------------------------------------------
// Leases
// ---------------------------------------------------------------------------

/// One per-Quota hold of a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseHold {
    /// The held Quota.
    pub quota_id: QuotaId,
    /// Held amount.
    pub held_amount: u64,
    /// Acquisition period row for consumption Quotas (invariant I5).
    pub period_id: Option<PeriodId>,
}

/// An expired lease reclaimed by the sweeper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiredLease {
    /// The lease token.
    pub token: LeaseToken,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Subject key persisted at acquisition.
    pub subject_key: IdempotencySubjectKey,
    /// Holds returned by the auto-release.
    pub holds: Vec<LeaseHold>,
    /// Expiry time.
    #[serde(with = "rfc3339")]
    pub expired_at: OffsetDateTime,
}

/// Item of a batch-debit envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchDebitItem {
    /// The item's applicable subject set.
    pub applicable: ApplicableQuotas,
    /// The item's debit plan.
    pub plan: DebitPlan,
    /// Optional per-item idempotency scope.
    #[serde(default)]
    pub item_scope: Option<IdempotencyScope>,
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// Current period bounds of a consumption Quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodWindow {
    /// Period start, inclusive.
    #[serde(with = "rfc3339")]
    pub start: OffsetDateTime,
    /// Period end, exclusive.
    #[serde(with = "rfc3339")]
    pub end: OffsetDateTime,
    /// Next reset time.
    #[serde(with = "rfc3339")]
    pub next_reset: OffsetDateTime,
}

/// Per-Quota state returned by snapshot reads (PRD section 5.10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    /// The Quota.
    pub quota_id: QuotaId,
    /// Bound subject.
    pub subject: SubjectRef,
    /// Metric.
    pub metric: MetricId,
    /// Accounting model.
    pub quota_type: QuotaType,
    /// Behaviour at the cap boundary.
    pub enforcement_mode: EnforcementMode,
    /// Cap. `None` means unbounded.
    pub cap: Option<u64>,
    /// Consumed amount, or in-flight amount for allocation Quotas.
    pub consumed: u64,
    /// Remaining capacity. `None` when the cap is unbounded.
    pub remaining: Option<u64>,
    /// Period bounds for consumption Quotas.
    pub period: Option<PeriodWindow>,
    /// Operator metadata.
    pub metadata: Map<String, Value>,
    /// Validity bounds.
    pub validity_window: Option<ValidityWindow>,
    /// Server-computed: the snapshot time lies within the validity window.
    pub currently_within_window: bool,
}

/// Outcome of a Quota deactivation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeactivateOutcome {
    /// Leases resolved atomically with the deactivation.
    pub resolved_leases: Vec<LeaseToken>,
}

// ---------------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------------

/// Scope of a Quota Resolution Policy. P1 is closed to two levels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyScope {
    /// Platform-wide fallback.
    Global,
    /// One metric.
    Metric {
        /// The metric.
        metric: MetricId,
    },
}

/// Create input for a Policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDraft {
    /// Scope.
    pub scope: PolicyScope,
    /// Registered engine identifier.
    pub engine_id: String,
    /// Engine-validated configuration.
    pub engine_config: Value,
    /// Per-policy evaluation timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Operator description.
    #[serde(default)]
    pub description: Option<String>,
    /// Version comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// Caller identity from the `SecurityContext`.
    pub created_by: String,
}

/// Update input for a Policy. Creates a new immutable version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyUpdate {
    /// Expected current version. Lost-update protection.
    pub if_match_version: u32,
    /// New engine identifier.
    #[serde(default)]
    pub engine_id: Option<String>,
    /// New engine configuration.
    #[serde(default)]
    pub engine_config: Option<Value>,
    /// New timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Version comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// Caller identity from the `SecurityContext`.
    pub created_by: String,
}

/// One immutable Policy version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersion {
    /// Stable policy identifier.
    pub policy_id: PolicyId,
    /// Monotonic version number, first version is 1.
    pub version: u32,
    /// Scope.
    pub scope: PolicyScope,
    /// Engine identifier.
    pub engine_id: String,
    /// Engine configuration.
    pub engine_config: Value,
    /// Evaluation timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Operator description.
    pub description: Option<String>,
    /// Version state.
    pub state: PolicyVersionState,
    /// Creation time.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// Creator identity.
    pub created_by: String,
    /// Version comment.
    pub comment: Option<String>,
}

/// Version listing entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionMeta {
    /// Version number.
    pub version: u32,
    /// Version state.
    pub state: PolicyVersionState,
    /// Creation time.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// Creator identity.
    pub created_by: String,
    /// Version comment.
    pub comment: Option<String>,
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

/// Platform default rows of the three configuration tables (DESIGN 3.7,
/// "Bootstrap seeded state").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigDefaults {
    /// Acquisition contention timeout. `0` means fail fast.
    pub contention_timeout_ms: u64,
    /// Per-`(tenant, metric)` active-lease cap (invariant I7).
    pub max_active_leases: u32,
    /// Idempotency record retention.
    pub idempotency_retention_secs: u64,
}

impl Default for ConfigDefaults {
    fn default() -> Self {
        Self {
            contention_timeout_ms: 0,
            max_active_leases: 1000,
            idempotency_retention_secs: 86_400,
        }
    }
}

/// Everything `bootstrap()` needs. Later features extend it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapBundle {
    /// Contract major the caller was compiled against (invariant I12).
    pub contract_major: u32,
    /// Default configuration rows to seed when missing.
    #[serde(default)]
    pub config_defaults: ConfigDefaults,
    /// Seeded `global` policy. `None` until the resolution-policy-engine
    /// feature registers its engine.
    #[serde(default)]
    pub global_policy: Option<PolicyDraft>,
}

impl BootstrapBundle {
    /// Foundation bundle: schema check and default configuration rows only.
    #[must_use]
    pub fn foundation() -> Self {
        Self {
            contract_major: crate::storage_plugin::CONTRACT_MAJOR,
            config_defaults: ConfigDefaults::default(),
            global_policy: None,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "models_tests.rs"]
mod models_tests;
