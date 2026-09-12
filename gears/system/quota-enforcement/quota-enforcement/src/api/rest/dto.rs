//! Wire shapes of the Quota lifecycle endpoints.
//!
//! GTS identifiers travel as strings and are parsed on the way into the
//! domain, so a malformed id is a canonical field violation rather than a
//! deserialization failure. Fields whose presence matters (`period` on create;
//! the immutable fields and the unbinding `cap`/`validity_window` on update)
//! are decoded presence-aware: an explicit `null` is present, an absent key is
//! not. Unknown keys are rejected everywhere.

use std::collections::BTreeMap;

use gts::GtsTypeId;
use quota_enforcement_sdk::{
    DeactivateOutcome, EnforcementMode, LeaseToken, PageResult, PeriodType, QuotaId, QuotaSource,
    QuotaStatus, QuotaType, QuotaView, SubjectRef, TenantId, ValidityWindow,
};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::quotas::{CreateQuotaRequest, ListQuotasRequest, Presence, UpdateQuotaRequest};
use crate::domain::tokens;

/// Absent key → `Absent` (through `#[serde(default)]`); present key → `Null`
/// for `null`, `Value` otherwise.
fn presence<'de, D, T>(deserializer: D) -> Result<Presence<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
        .map(|value| value.map_or(Presence::Null, Presence::Value))
}

fn invalid(field: &'static str, reason: &'static str) -> DomainError {
    DomainError::InvalidArgument { field, reason }
}

/// A subject reference on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct SubjectRefDto {
    /// Concrete owner projection type id.
    pub projection_type: String,
    /// Subject identifier.
    pub subject_id: String,
}

impl From<SubjectRef> for SubjectRefDto {
    fn from(subject: SubjectRef) -> Self {
        Self {
            projection_type: subject.projection_type.as_ref().to_owned(),
            subject_id: subject.subject_id,
        }
    }
}

impl TryFrom<SubjectRefDto> for SubjectRef {
    type Error = DomainError;

    fn try_from(dto: SubjectRefDto) -> Result<Self, Self::Error> {
        let projection_type = GtsTypeId::try_new(&dto.projection_type)
            .map_err(|_| invalid("subject.projection_type", tokens::PROJECTION_INVALID))?;
        Ok(Self {
            projection_type,
            subject_id: dto.subject_id,
        })
    }
}

/// Validity bounds on the wire, RFC 3339, both ends inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct ValidityWindowDto {
    /// Start, inclusive; absent or `null` for no lower bound.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>)]
    pub start: Option<OffsetDateTime>,
    /// End, inclusive; absent or `null` for no upper bound.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>)]
    pub end: Option<OffsetDateTime>,
}

impl From<ValidityWindow> for ValidityWindowDto {
    fn from(window: ValidityWindow) -> Self {
        Self {
            start: window.start,
            end: window.end,
        }
    }
}

impl From<ValidityWindowDto> for ValidityWindow {
    fn from(dto: ValidityWindowDto) -> Self {
        Self {
            start: dto.start,
            end: dto.end,
        }
    }
}

/// Body of `POST /quotas`. No `constraint_contract`: the gear resolves it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateQuotaDto {
    /// Explicit target tenant.
    pub tenant_id: Uuid,
    /// Explicit target subject.
    pub subject: SubjectRefDto,
    /// Registered metric instance id.
    pub metric: String,
    /// Quota type instance id under `gts.cf.qe.quota.type.v1~`; `rate` is
    /// reserved and answers `501`.
    #[schema(value_type = String)]
    pub quota_type: QuotaType,
    /// Period instance id under `gts.cf.qe.period.type.v1~`. Required for
    /// consumption Quotas, rejected for allocation Quotas (`null` included).
    #[serde(default, deserialize_with = "presence")]
    #[schema(value_type = Option<String>)]
    pub period: Presence<PeriodType>,
    /// Enforcement mode instance id under `gts.cf.qe.enforcement.type.v1~`.
    #[schema(value_type = String)]
    pub enforcement_mode: EnforcementMode,
    /// Cap in metric units within `0..=9223372036854775807`; absent or
    /// `null` for unbounded.
    #[serde(default)]
    pub cap: Option<i64>,
    /// Notification thresholds as percentages of cap, ascending.
    #[serde(default)]
    pub notification_thresholds: Vec<u8>,
    /// Optional validity bounds.
    #[serde(default)]
    pub validity_window: Option<ValidityWindowDto>,
    /// Informational fail-open hint.
    #[serde(default)]
    pub fail_open_hint: bool,
    /// Metadata validated against the metric owner's constraint contract.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Map<String, Value>>,
    /// Source instance id under `gts.cf.qe.source.type.v1~`.
    #[schema(value_type = String)]
    pub source: QuotaSource,
}

impl TryFrom<CreateQuotaDto> for CreateQuotaRequest {
    type Error = DomainError;

    fn try_from(dto: CreateQuotaDto) -> Result<Self, Self::Error> {
        Ok(Self {
            tenant_id: TenantId::new(dto.tenant_id),
            subject: SubjectRef::try_from(dto.subject)?,
            metric: dto.metric,
            quota_type: dto.quota_type,
            period: dto.period,
            enforcement_mode: dto.enforcement_mode,
            cap: dto.cap,
            notification_thresholds: dto.notification_thresholds,
            validity_window: dto.validity_window.map(ValidityWindow::from),
            fail_open_hint: dto.fail_open_hint,
            metadata: dto.metadata,
            source: dto.source,
        })
    }
}

/// Body of `PATCH /quotas/{id}`. The immutable fields are accepted so their
/// rejection is precise: `quota_type` naming `rate` answers `501`, any other
/// present immutable field answers `400 IMMUTABLE_FIELD`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpdateQuotaDto {
    /// Immutable; rejected when present.
    #[serde(default, deserialize_with = "presence")]
    #[schema(value_type = Option<Value>)]
    pub metric: Presence<Value>,
    /// Immutable; rejected when present.
    #[serde(default, deserialize_with = "presence")]
    #[schema(value_type = Option<Value>)]
    pub quota_type: Presence<Value>,
    /// Immutable; rejected when present.
    #[serde(default, deserialize_with = "presence")]
    #[schema(value_type = Option<Value>)]
    pub period: Presence<Value>,
    /// Immutable; rejected when present.
    #[serde(default, deserialize_with = "presence")]
    #[schema(value_type = Option<Value>)]
    pub subject: Presence<Value>,
    /// New cap; `null` unbinds it.
    #[serde(default, deserialize_with = "presence")]
    #[schema(value_type = Option<i64>)]
    pub cap: Presence<i64>,
    /// New thresholds, replacing the list; `[]` clears it.
    #[serde(default)]
    pub notification_thresholds: Option<Vec<u8>>,
    /// New validity window; `null` clears it.
    #[serde(default, deserialize_with = "presence")]
    #[schema(value_type = Option<ValidityWindowDto>)]
    pub validity_window: Presence<ValidityWindowDto>,
    /// New metadata object, replacing the previous one.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Map<String, Value>>,
    /// New enforcement mode.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub enforcement_mode: Option<EnforcementMode>,
    /// New fail-open hint.
    #[serde(default)]
    pub fail_open_hint: Option<bool>,
}

impl From<UpdateQuotaDto> for UpdateQuotaRequest {
    fn from(dto: UpdateQuotaDto) -> Self {
        let validity_window = match dto.validity_window {
            Presence::Absent => Presence::Absent,
            Presence::Null => Presence::Null,
            Presence::Value(window) => Presence::Value(ValidityWindow::from(window)),
        };
        Self {
            metric: dto.metric,
            quota_type: dto.quota_type,
            period: dto.period,
            subject: dto.subject,
            cap: dto.cap,
            notification_thresholds: dto.notification_thresholds,
            validity_window,
            metadata: dto.metadata,
            enforcement_mode: dto.enforcement_mode,
            fail_open_hint: dto.fail_open_hint,
        }
    }
}

/// Query of `GET /quotas`. Repeated `id` keys collect into a list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, toolkit_contract::QueryParams)]
pub struct ListQuotasQuery {
    /// Owning tenant UUID; the caller's own tenant when absent.
    pub tenant_id: Option<String>,
    /// Subject projection type id; must come with `subject_id`.
    pub projection_type: Option<String>,
    /// Subject identifier; must come with `projection_type`.
    pub subject_id: Option<String>,
    /// Metric instance id.
    pub metric: Option<String>,
    /// `active` or `deactivated`.
    pub status: Option<String>,
    /// Quota UUIDs; repeat the key for several.
    #[serde(default)]
    pub id: Vec<String>,
    /// Page size.
    pub limit: Option<u32>,
    /// Opaque continuation cursor from a previous page.
    pub cursor: Option<String>,
}

impl TryFrom<ListQuotasQuery> for ListQuotasRequest {
    type Error = DomainError;

    fn try_from(query: ListQuotasQuery) -> Result<Self, Self::Error> {
        let tenant_id = query
            .tenant_id
            .map(|raw| {
                Uuid::parse_str(&raw)
                    .map(TenantId::new)
                    .map_err(|_| invalid("tenant_id", tokens::TENANT_ID_INVALID))
            })
            .transpose()?;
        let projection_type = query
            .projection_type
            .map(|raw| {
                GtsTypeId::try_new(&raw)
                    .map_err(|_| invalid("projection_type", tokens::PROJECTION_INVALID))
            })
            .transpose()?;
        let status = query
            .status
            .map(|raw| match raw.as_str() {
                "active" => Ok(QuotaStatus::Active),
                "deactivated" => Ok(QuotaStatus::Deactivated),
                _ => Err(invalid("status", tokens::STATUS_INVALID)),
            })
            .transpose()?;
        let ids = query
            .id
            .iter()
            .map(|raw| {
                Uuid::parse_str(raw)
                    .map(QuotaId::new)
                    .map_err(|_| invalid("id", tokens::QUOTA_ID_INVALID))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            tenant_id,
            projection_type,
            subject_id: query.subject_id,
            metric: query.metric,
            status,
            ids,
            limit: query.limit,
            cursor: query.cursor,
        })
    }
}

/// The contract reference snapshotted with a Quota.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ContractRefDto {
    /// Constraint contract type id.
    pub type_id: String,
    /// Major version of the contract.
    pub version: u32,
}

/// The public view of a Quota: the stored record plus the server-computed
/// facts.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaViewDto {
    /// Server-assigned identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Bound subject.
    pub subject: SubjectRefDto,
    /// Metric instance id.
    pub metric: String,
    /// Quota type instance id.
    pub quota_type: String,
    /// Period instance id, consumption Quotas only.
    pub period: Option<String>,
    /// Enforcement mode instance id.
    pub enforcement_mode: String,
    /// Cap; `null` for unbounded.
    pub cap: Option<u64>,
    /// Notification thresholds as percentages of cap.
    pub notification_thresholds: Vec<u8>,
    /// Validity bounds.
    pub validity_window: Option<ValidityWindowDto>,
    /// Informational fail-open hint.
    pub fail_open_hint: bool,
    /// The full metadata object.
    #[schema(value_type = Object)]
    pub metadata: BTreeMap<String, Value>,
    /// Source instance id.
    pub source: String,
    /// `active` or `deactivated`.
    pub status: String,
    /// The contract the metadata was validated against.
    pub constraint_contract: ContractRefDto,
    /// Record version.
    pub record_version: u32,
    /// Creation time, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String)]
    pub created_at: OffsetDateTime,
    /// Last mutation time, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String)]
    pub updated_at: OffsetDateTime,
    /// Server-computed: the response time lies within the validity window.
    pub currently_within_window: bool,
    /// Registry-reported metric kind, `counter` or `gauge`; `null` only when
    /// the registry no longer knows the metric.
    pub metric_kind: Option<String>,
}

impl From<QuotaView> for QuotaViewDto {
    fn from(view: QuotaView) -> Self {
        let quota = view.quota;
        Self {
            id: quota.id.as_uuid(),
            tenant_id: quota.tenant_id.as_uuid(),
            subject: SubjectRefDto::from(quota.subject),
            metric: quota.metric.as_str().to_owned(),
            quota_type: quota.quota_type.as_gts_id().to_owned(),
            period: quota.period.map(|p| p.as_gts_id().to_owned()),
            enforcement_mode: quota.enforcement_mode.as_gts_id().to_owned(),
            cap: quota.cap,
            notification_thresholds: quota.notification_thresholds,
            validity_window: quota.validity_window.map(ValidityWindowDto::from),
            fail_open_hint: quota.fail_open_hint,
            metadata: quota.metadata.into_iter().collect(),
            source: quota.source.as_gts_id().to_owned(),
            status: match quota.status {
                QuotaStatus::Active => "active".to_owned(),
                QuotaStatus::Deactivated => "deactivated".to_owned(),
            },
            constraint_contract: ContractRefDto {
                type_id: quota.constraint_contract.type_id.as_ref().to_owned(),
                version: quota.constraint_contract.version,
            },
            record_version: quota.record_version,
            created_at: quota.created_at,
            updated_at: quota.updated_at,
            currently_within_window: view.currently_within_window,
            metric_kind: view.metric_kind.map(|kind| {
                serde_json::to_value(kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            }),
        }
    }
}

/// One page of Quotas.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPageDto {
    /// The items of this page.
    pub items: Vec<QuotaViewDto>,
    /// Cursor of the next page; `null` on the last page.
    pub next_cursor: Option<String>,
}

impl From<PageResult<QuotaView>> for QuotaPageDto {
    fn from(page: PageResult<QuotaView>) -> Self {
        Self {
            items: page.items.into_iter().map(QuotaViewDto::from).collect(),
            next_cursor: page.next_cursor,
        }
    }
}

/// Outcome of `POST /quotas/{id}/deactivate`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DeactivateOutcomeDto {
    /// Leases resolved atomically with the deactivation.
    pub resolved_leases: Vec<Uuid>,
}

impl From<DeactivateOutcome> for DeactivateOutcomeDto {
    fn from(outcome: DeactivateOutcome) -> Self {
        Self {
            resolved_leases: outcome
                .resolved_leases
                .into_iter()
                .map(LeaseToken::as_uuid)
                .collect(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "dto_tests.rs"]
mod dto_tests;
