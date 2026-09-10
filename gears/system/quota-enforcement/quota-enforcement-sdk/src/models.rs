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
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::str::FromStr;

use gts::{GtsId, GtsIdError, GtsInstanceId, GtsTypeId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::serde::rfc3339;
use uuid::Uuid;

use crate::gts::{SCOPE_TENANT, SCOPE_TYPE, SCOPE_USER};

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

impl NotificationEventKind {
    /// Stable `kebab-case` name, equal to the serialized form; the outbox
    /// stores it as the payload type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThresholdCrossed => "threshold-crossed",
            Self::PeriodRollover => "period-rollover",
            Self::LeaseAutoReleased => "lease-auto-released",
            Self::LeaseResolvedByDeactivation => "lease-resolved-by-deactivation",
            Self::QuotaChanged => "quota-changed",
            Self::QuotaCounterAdjusted => "quota-counter-adjusted",
            Self::QuotaRollbackApplied => "quota-rollback-applied",
            Self::PolicyChanged => "policy-changed",
        }
    }
}

/// Registry-reported kind of a metric (PRD section 3.2). Closed; the gear
/// reads it from the metric instance and never defaults it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// Cumulative within a period; pairs naturally with consumption Quotas.
    Counter,
    /// A level; pairs naturally with allocation Quotas.
    Gauge,
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

impl ContractRef {
    /// The reference to `type_id` at the version its last segment declares.
    ///
    /// Returns `None` when the id does not parse or its last segment carries
    /// no major version, which a registry-validated type id never does.
    #[must_use]
    pub fn for_type(type_id: &GtsTypeId) -> Option<Self> {
        let version = GtsId::try_new(type_id.as_ref())
            .ok()?
            .segments()
            .last()?
            .ver_major_opt()?;
        Some(Self {
            type_id: type_id.clone(),
            version,
        })
    }
}

/// A well-known instance of the scope-discriminator type
/// `gts.cf.core.qe.scope.v1~` (ADR-0007). The value is an identity-only
/// discriminator: it names no `SecurityContext` accessor, and the gear compares
/// instance ids directly.
///
/// The invariant "an instance of the scope type" holds for every value: the
/// constructors validate it, and deserialization goes through [`Self::parse`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct SubjectScope(GtsInstanceId);

impl SubjectScope {
    /// Parse a scope instance id.
    ///
    /// # Errors
    ///
    /// Returns [`ScopeError`] when `raw` is not a well-formed GTS instance id
    /// or its declaring type is not the scope-discriminator type.
    pub fn parse(raw: &str) -> Result<Self, ScopeError> {
        let parsed = GtsId::try_new(raw).map_err(|e| ScopeError::Invalid {
            id: raw.to_owned(),
            detail: e.to_string(),
        })?;
        if parsed.is_type() || parsed.get_type_id().as_deref() != Some(SCOPE_TYPE) {
            return Err(ScopeError::NotAScope { id: raw.to_owned() });
        }
        GtsInstanceId::try_new(raw)
            .map(Self)
            .map_err(|e| ScopeError::Invalid {
                id: raw.to_owned(),
                detail: e.to_string(),
            })
    }

    /// The `user` scope.
    #[must_use]
    pub fn user() -> Self {
        Self(GtsInstanceId::new(SCOPE_TYPE, user_segment()))
    }

    /// The `tenant` scope, materialized from every request's `tenant_id`.
    #[must_use]
    pub fn tenant() -> Self {
        Self(GtsInstanceId::new(SCOPE_TYPE, tenant_segment()))
    }

    /// True for the `tenant` scope.
    #[must_use]
    pub fn is_tenant(&self) -> bool {
        self.0 == SCOPE_TENANT
    }

    /// The underlying instance id.
    #[must_use]
    pub const fn as_gts(&self) -> &GtsInstanceId {
        &self.0
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

/// The instance segment of a well-known scope id: the text after the type id.
fn user_segment() -> &'static str {
    &SCOPE_USER[SCOPE_TYPE.len()..]
}

fn tenant_segment() -> &'static str {
    &SCOPE_TENANT[SCOPE_TYPE.len()..]
}

impl TryFrom<String> for SubjectScope {
    type Error = ScopeError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl Serialize for SubjectScope {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl fmt::Display for SubjectScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A value that is not a scope instance.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    /// Not a well-formed GTS instance id.
    #[error("{id} is not a GTS instance id: {detail}")]
    Invalid {
        /// The rejected text.
        id: String,
        /// The parser's diagnostic.
        detail: String,
    },
    /// A well-formed id whose declaring type is not `gts.cf.core.qe.scope.v1~`.
    #[error("{id} is not an instance of the QE scope type")]
    NotAScope {
        /// The rejected id.
        id: String,
    },
}

/// One caller-supplied subject: a scope `kind` and an opaque `id`.
///
/// `kind` travels as text so a malformed value becomes a canonical
/// `InvalidArgument` at ingress rather than a deserialization failure. The
/// gear maps `(metric, kind)` to the owner projection; callers never name a
/// projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectClaim {
    /// A well-known instance of the scope-discriminator type, as text.
    pub kind: String,
    /// Opaque, non-empty subject identifier.
    pub id: String,
}

/// An optional resource projection on the wire (ADR-0007): the complete
/// `{type, id?, metadata}` document validated against the owner contract.
///
/// `id` and `metadata` distinguish omission from an explicit `null`: the
/// resource base allows an omitted `id` and requires a string when present, and
/// requires `metadata`. An omitted field is `None`; `null` fails
/// deserialization; `None` is never written as `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceProjection {
    /// The concrete resource projection type, as text.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Optional resource identity.
    #[serde(
        default,
        deserialize_with = "present_or_error",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<String>,
    /// Schematized resource properties. Required by the resource base.
    #[serde(
        default,
        deserialize_with = "present_or_error",
        skip_serializing_if = "Option::is_none"
    )]
    pub metadata: Option<Map<String, Value>>,
}

/// The caller-supplied attribution of one subject-based evaluation request
/// (debit, reserve, preview, each batch item). Untrusted until the PDP
/// authorizes the complete tuple for the authenticated service principal.
///
/// `metadata` is required on the wire, including `{}` when the request
/// contract declares no properties; it is `Option` only so the gear can tell an
/// omitted object from a present one and reject the omission instead of
/// defaulting it. `null` fails deserialization for every optional field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationAttribution {
    /// Target tenant. The tenant-scope subject is materialized from it.
    pub tenant_id: TenantId,
    /// The metric, as text: a registered instance of the platform metric base.
    pub metric: String,
    /// Additional subjects. The tenant scope must not be repeated here.
    #[serde(default)]
    pub subjects: Vec<SubjectClaim>,
    /// The operation-level metadata object, validated against the metric's
    /// request contract.
    #[serde(
        default,
        deserialize_with = "present_or_error",
        skip_serializing_if = "Option::is_none"
    )]
    pub metadata: Option<Map<String, Value>>,
    /// Optional resource projection.
    #[serde(
        default,
        deserialize_with = "present_or_error",
        skip_serializing_if = "Option::is_none"
    )]
    pub resource: Option<ResourceProjection>,
}

/// Deserializes a present field as `Some`, so that `null` is an error rather
/// than `None`. Pair with `#[serde(default)]` so an omitted field is `None`.
fn present_or_error<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// The `(metric, projection_type)` pair an active Quota binds. Storage reports
/// the distinct set at bootstrap so the configured catalogue can be checked
/// against it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectionBinding {
    /// The Quota's metric.
    pub metric: MetricId,
    /// The subject projection the Quota is bound to.
    pub projection_type: GtsTypeId,
}

/// Active-Quota counts behind the lifecycle gauges (PRD section 5.16):
/// `quota_cap_zero_total`, `quota_cap_unbounded_total`, and, joined with the
/// current metric classification by the gear, `quota_for_direct_metric_total`.
/// Platform-plane and caller-less, like [`ProjectionBinding`]; only Quotas with
/// lifecycle status `active` count, whatever their validity window.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ActiveQuotaCounts {
    /// Active Quotas with `cap = 0`.
    pub cap_zero: u64,
    /// Active Quotas with an unbounded cap.
    pub cap_unbounded: u64,
    /// Active Quotas per metric.
    pub by_metric: HashMap<MetricId, u64>,
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
    /// Cap in metric units, within `0..=`[`Quota::MAX_CAP`]. `None` means
    /// unbounded.
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
    /// Contract the metadata was validated against: snapshotted at creation and
    /// moved with every accepted metadata update.
    pub constraint_contract: ContractRef,
    /// Record version. Increments once per accepted mutation.
    pub record_version: u32,
    /// Creation time.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last mutation time.
    #[serde(with = "rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl Quota {
    /// Largest cap any surface accepts. Caps live in `0..=i64::MAX` so every
    /// storage backend holds them in a signed 64-bit column: the REST surface
    /// takes a signed integer and rejects negatives, the SDK checks `u64`
    /// values against this bound (`CAP_OUT_OF_RANGE`), SQL carries a check
    /// constraint.
    pub const MAX_CAP: u64 = i64::MAX.unsigned_abs();
}

/// The public read shape of a Quota: the stored record plus what the gear
/// computes at read time. Every Quota read and list returns it, over REST and
/// in process alike (quota-lifecycle feature). Storage never produces it; its
/// `read_quotas` returns the bare [`Quota`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaView {
    /// The stored record.
    #[serde(flatten)]
    pub quota: Quota,
    /// Server-computed: the response's clock reading lies within
    /// `validity_window`, an absent bound being unbounded on its side. One
    /// clock reading per response.
    pub currently_within_window: bool,
    /// Registry-reported kind of the metric at read time. `None` only when
    /// the registry no longer knows the metric; the record stays readable and
    /// the removal is logged.
    #[serde(default)]
    pub metric_kind: Option<MetricKind>,
}

impl QuotaView {
    /// Pure computation of the view for the clock reading `now`.
    #[must_use]
    pub fn compute(quota: Quota, metric_kind: Option<MetricKind>, now: OffsetDateTime) -> Self {
        let currently_within_window = quota.validity_window.is_none_or(|w| w.contains(now));
        Self {
            quota,
            currently_within_window,
            metric_kind,
        }
    }
}

/// Public create input of a Quota (`QuotaManagerClientV1::create_quota`): a
/// [`QuotaDraft`] without the constraint contract, which the gear resolves from
/// the catalogue and snapshots itself. The gear validates every field before
/// storage sees a draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaSpec {
    /// PDP-authorized target tenant.
    pub tenant_id: TenantId,
    /// Subject the Quota binds to: `(projection_type, subject_id)`.
    pub subject: SubjectRef,
    /// Registered metric.
    pub metric: MetricId,
    /// Accounting model. `rate` is reserved and rejected.
    pub quota_type: QuotaType,
    /// Period specification. Required for consumption Quotas, rejected for
    /// allocation Quotas.
    #[serde(default)]
    pub period: Option<PeriodType>,
    /// Behaviour at the cap boundary.
    pub enforcement_mode: EnforcementMode,
    /// Cap in metric units, within `0..=`[`Quota::MAX_CAP`]. `None` means
    /// unbounded.
    #[serde(default)]
    pub cap: Option<u64>,
    /// Notification thresholds as percentages of cap, ascending. Require a
    /// bounded cap.
    #[serde(default)]
    pub notification_thresholds: Vec<u8>,
    /// Optional validity bounds.
    #[serde(default)]
    pub validity_window: Option<ValidityWindow>,
    /// Informational fail-open hint. Defaults to fail-closed.
    #[serde(default)]
    pub fail_open_hint: bool,
    /// Metadata validated against the metric owner's constraint contract.
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// Who imposes the Quota.
    pub source: QuotaSource,
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
    /// Cap in metric units, within `0..=`[`Quota::MAX_CAP`]. `None` means
    /// unbounded.
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
    /// Contract the metadata was validated against. Set by the gear from the
    /// catalogue, never caller-supplied: the public create shape is
    /// [`QuotaSpec`], which has no such field.
    pub constraint_contract: ContractRef,
}

impl QuotaDraft {
    /// The storage draft of a validated `spec` with the resolved contract.
    #[must_use]
    pub fn from_spec(spec: QuotaSpec, constraint_contract: ContractRef) -> Self {
        Self {
            tenant_id: spec.tenant_id,
            subject: spec.subject,
            metric: spec.metric,
            quota_type: spec.quota_type,
            period: spec.period,
            enforcement_mode: spec.enforcement_mode,
            cap: spec.cap,
            notification_thresholds: spec.notification_thresholds,
            validity_window: spec.validity_window,
            fail_open_hint: spec.fail_open_hint,
            metadata: spec.metadata,
            source: spec.source,
            constraint_contract,
        }
    }
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
    /// The contract the new `metadata` was validated against, stored with it in
    /// the same write so the reference never lags the object. Set by the gear
    /// whenever `metadata` is present, never caller-supplied: the gear rejects a
    /// present value from a client. Storage rejects a `metadata` patch without
    /// it as `Internal`.
    pub constraint_contract: Option<ContractRef>,
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "models_tests.rs"]
mod models_tests;
