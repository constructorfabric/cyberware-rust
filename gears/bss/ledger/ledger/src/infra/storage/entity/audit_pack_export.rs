//! `SeaORM` entity for `bss.audit_pack_export` — one materialized audit-pack
//! export job (Slice 6 §5/§10). `POST …/audit/packs` creates a row and returns
//! `202 Accepted` + a `Location` to `GET …/audit/packs/{exportId}`, which polls
//! this row for the job `status` and, once `succeeded`, the materialized CSV.
//!
//! The row is owned by the **requester's home tenant** (`tenant_id`) — the same
//! tenant the cross-tenant-access forensic record is written under — so the
//! requester polls it under its own scope. `target_tenant_id` records whose
//! ledger was opened (equal to `tenant_id` on a routine same-tenant export).
//!
//! **MVP is contract-only (§10 interim).** The CSV is still built synchronously
//! in the create request, so a created row is born `succeeded` with its `csv`
//! set. The wire contract (202 + `Location` + polling) is the durable part; a
//! background worker that flips `accepted` → `processing` → `succeeded` is a
//! future extension that needs no migration (the `status` values already model
//! it).
//!
//! `SQLite` (the non-production test backend) mirrors the shape with the
//! systematic transforms (drop the `bss.` prefix; `uuid` → `text`;
//! `bytea` → `blob`; `timestamptz` → `text`).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "audit_pack_export")]
#[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub export_id: Uuid,
    /// Owner tenant = the requester's home tenant (the row is polled under this
    /// scope).
    pub tenant_id: Uuid,
    /// The tenant whose ledger was exported (= `tenant_id` for a routine
    /// same-tenant export; a different tenant on the forensic cross-tenant path).
    pub target_tenant_id: Uuid,
    /// `accepted` | `processing` | `succeeded` | `failed`. MVP rows are born
    /// `succeeded` (the build is synchronous); the other states are reserved for
    /// the future background-worker path.
    pub status: String,
    /// Machine-readable investigation reason code (cross-tenant exports only).
    pub reason_code: Option<String>,
    /// The authenticated subject that requested the export.
    pub actor_ref: String,
    /// The materialized CSV document (UTF-8 bytes); present once `succeeded`.
    pub csv: Option<Vec<u8>>,
    /// Data-row count of the CSV (excludes the header row).
    pub row_count: i64,
    /// Failure diagnostic when `status = failed` (id-only / no PII).
    pub error_detail: Option<String>,
    pub created_at_utc: OffsetDateTime,
    /// Set when the job reaches a terminal state (`succeeded` / `failed`).
    pub completed_at_utc: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
