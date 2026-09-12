//! `SeaORM` entity for `bss.pricing_approval` — the approval workflow's record
//! (`design/05-governance.md` §6).
//!
//! Four columns are nullable and they are the same four the decision writes, so
//! their absence has one meaning between them: the record is still `submitted`.
//! The schema says it twice over — `chk_pricing_approval_decided_at` makes
//! `state = 'submitted'` and `decided_at IS NULL` the same fact, and
//! `chk_pricing_approval_approver` and `chk_pricing_approval_reason` bind the
//! other two to the outcome — which is why nothing here reconstructs pendingness
//! from a null. [`crate::domain::approval::ApprovalState`] is the reading, and
//! `state` is what carries it.
//!
//! `content_hash` is `Vec<u8>` rather than a hex `String`: it is a digest, the
//! column is `bytea` on Postgres and `blob` on `SQLite`, and a text rendering
//! would give one value two spellings for the comparison `inst-ap-pin` makes at
//! decision time.
//!
//! The `resource_col` is `approval_id`, so a scope naming one approval reaches
//! that row and no other — the same shape `pricing_policy_object` uses with its
//! tenant key.


use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_approval")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "approval_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub approval_id: Uuid,
    pub tenant_id: Uuid,
    /// The pinned subject's durable name — `<plan_id>/<revision>` for a plan
    /// revision, the `window_id` for a window mutation. `text` rather than S5 §6's
    /// `uuid` because a plan revision is not named by one; see the migration's
    /// module doc.
    ///
    /// A `price_unit`'s form is **undecided**, because the kind has no writer: the
    /// bare `price_id` this doc used to promise is not a form
    /// `approval_repo::subject_plan` can read, so the slice that gives the kind a
    /// writer owes `subject_aggregate` an arm. Reported there as well.
    pub subject_ref: String,
    /// `AuditSubjectKind`'s enumeration, which D-158 requires this store to spell
    /// identically and to extend in step.
    ///
    /// The members are **not** listed here. They were, and the list went stale the
    /// day `window` arrived: the type is the roster, `chk_pricing_approval_subject_kind`
    /// is its storage half, and a third copy in a column doc is one more place for
    /// them to disagree.
    pub subject_kind: String,
    /// The digest of the content pinned at submission (`inst-ap-pin`).
    pub content_hash: Vec<u8>,
    /// `submitted` | `approved` | `rejected` | `voided`.
    pub state: String,
    /// Who opened the record — identity, never role (`inst-tp-distinct`).
    pub submitter_principal: Uuid,
    /// Who decided it. `None` while `submitted`, and `None` forever on a
    /// `voided` record: a TOCTOU void has no human decider, and a withdraw's is
    /// the submitter, whom `chk_pricing_approval_distinct_principals` forbids
    /// here.
    pub approver_principal: Option<Uuid>,
    /// Mandatory on a reject, and on nothing else.
    pub reason: Option<String>,
    /// The materiality evaluator's output — per-currency deltas, tripped rows,
    /// trigger source (`jsonb` on Postgres, `text` on `SQLite`).
    pub materiality: Json,
    pub submitted_at: OffsetDateTime,
    /// `None` exactly while the record is `submitted`.
    pub decided_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
