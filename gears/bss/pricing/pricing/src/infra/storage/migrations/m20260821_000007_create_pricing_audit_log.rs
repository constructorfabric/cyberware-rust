//! Create `bss.pricing_audit_log` — the append-only, hash-chained actor /
//! before-after / approval trail (`design/01-foundation.md` §3.7; normative in
//! `design/05-governance.md`), retained for at least seven years.
//!
//! The chain is **segmented per `(tenant_id, chain_id)`** where `chain_id` is
//! the audited subject's aggregate — plan, overlay, payer, policy, bulk
//! operation (**D-135**). A single per-tenant chain would have serialized
//! *every* mutation of a tenant behind one head, inside the mutation
//! transaction, which the >= 50 rows/s repricing SLO never accounted for.
//! Tamper-evidence across segments is restored by a **periodic per-tenant
//! roll-up row** (`entry_kind = 'rollup'`) whose `segment_heads` payload chains
//! the segment heads together.
//!
//! **The actor is a pseudonymous principal id, never a display name or an email
//! address** (D-61, `inst-au-pii`). The column is typed `uuid` rather than
//! `text` precisely so that constraint is physical: a retention horizon of
//! seven-plus years must not accumulate directly identifying operator PII, and
//! a `text` column would eventually be handed one. Resolving a principal id to
//! a human is the identity plane's job, subject to its own access control.
//!
//! The append-only trigger rejects **every** UPDATE and
//! **every** DELETE unconditionally — unlike `pricing_plan` and `pricing_price`
//! there is no whitelist here, because there is no sanctioned in-place mutation
//! of an audit record at all; a hash chain whose links can be rewritten is not
//! evidence. Beside it stand the `CHECK`s the `CREATE TABLE` below spells.
//! `chk_pricing_audit_log_rollup` ties `entry_kind` to `segment_heads`, so a
//! roll-up row cannot exist without the heads it claims to chain, nor a mutation
//! row carry heads it never computed; `chk_pricing_audit_log_entry_kind` and
//! `chk_pricing_audit_log_seq` hold the kind roster and the sequence floor; and
//! `chk_pricing_audit_log_action` and `chk_pricing_audit_log_subject_kind` are
//! the two vocabulary rosters, argued below.
//!
//! **Backend differences.** Postgres raises through PL/pgSQL with the offending
//! `TG_OP` interpolated; `SQLite` has no procedural language and
//! `RAISE(ABORT, ...)` takes a literal message, so the mirror is two triggers
//! with fixed messages. `bytea` becomes `blob` and `jsonb` becomes `text`. As
//! elsewhere in this chain, `REVOKE UPDATE, DELETE` is not issued: it names a
//! deployment role this migration does not own, and `SQLite` has no
//! `GRANT`/`REVOKE`.
//!
//! # Both vocabulary columns are constrained
//!
//! `pricing_audit_log.subject_kind`, `pricing_approval.subject_kind` and
//! `pricing_audit_log.action` each hold **one** vocabulary from **one** Rust enum —
//! `domain::audit::AuditSubjectKind` for the two subject columns,
//! `domain::audit::AuditAction` for the action. D-158 requires the two
//! `subject_kind` stores to spell their enumeration identically and to extend it in
//! step — `entity/approval.rs` states that as a rule. Each `IN` list here is that
//! enum's `ALL` `as_str` tokens, in the enum's own order.
//!
//! Keeping them in step is not this file's promise but a test's:
//! `sqlite_audit_chain::every_subject_kind_d158_declares_is_storable_in_the_trail`
//! and `sqlite_audit_chain::every_action_the_domain_declares_is_storable_in_the_trail`
//! iterate `AuditSubjectKind::ALL` and `AuditAction::ALL` and insert one row per
//! token against these very CHECKs, so a new variant reddens on the next
//! `cargo test` with nothing else touched.
//!
//! **The constraints are physical even though the only writer is typed.**
//! `audit_repo::append` is the single site that builds an `audit_log::ActiveModel`
//! and it writes `entry.subject_kind.as_str()` and `entry.action.as_str()` off the
//! enums, so no unspelled token can arrive through the crate as it stands. The
//! reason to make them physical anyway is what the table *is*: a hash-chained
//! record retained seven-plus years, immutable by trigger and by design. It is the
//! last place a token should be able to arrive unspelled and the one place a wrong
//! one cannot be corrected afterwards — a vocabulary column fixed by an `UPDATE` is
//! a broken chain, and one fixed by a `DELETE` plus re-insert is a re-written one.
//!
//! Leaving `action` free-form instead costs the column the property it exists for.
//! The trail is what an operator reads years later, and a free column accumulates
//! per-site spellings of one act — `publish` beside `published` beside
//! `plan.publish` — which no query over `action` can reconcile and no `UPDATE` can
//! repair here.
//!
//! Dependency level 0.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const PG_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE bss.pricing_audit_log (
            tenant_id          uuid        NOT NULL,
            chain_id           uuid        NOT NULL,
            seq                bigint      NOT NULL,
            action             text        NOT NULL,
            actor_principal_id uuid        NOT NULL,
            after_state        jsonb,
            approval_ref       uuid,
            before_state       jsonb,
            correlation_id     uuid,
            entry_kind         text        NOT NULL DEFAULT 'mutation'::text,
            prev_hash          bytea,
            recorded_at        timestamptz NOT NULL DEFAULT now(),
            row_hash           bytea       NOT NULL,
            segment_heads      jsonb,
            subject_kind       text        NOT NULL,
            subject_ref        text        NOT NULL,
            CONSTRAINT chk_pricing_audit_log_action CHECK (action IN ('create','update','delete','abandon','publish','submit','approve','reject','withdraw','deny','retire','migrate')),
            CONSTRAINT chk_pricing_audit_log_entry_kind CHECK (entry_kind IN ('mutation','rollup')),
            CONSTRAINT chk_pricing_audit_log_rollup CHECK ((entry_kind = 'rollup') = (segment_heads IS NOT NULL)),
            CONSTRAINT chk_pricing_audit_log_seq CHECK (seq >= 0),
            CONSTRAINT chk_pricing_audit_log_subject_kind CHECK (subject_kind IN ('plan_revision','price_unit','window','policy','overlay','bulk_operation','membership','taxonomy_value')),
            CONSTRAINT pricing_audit_log_pkey PRIMARY KEY (tenant_id, chain_id, seq)
        )",
    "CREATE INDEX idx_pricing_audit_log_recorded ON bss.pricing_audit_log USING btree (tenant_id, recorded_at)",
    "CREATE INDEX idx_pricing_audit_log_subject ON bss.pricing_audit_log USING btree (tenant_id, subject_kind, subject_ref, recorded_at)",
    "CREATE OR REPLACE FUNCTION bss.pricing_audit_log_append_only() RETURNS trigger AS $$
        BEGIN
          RAISE EXCEPTION 'pricing_audit_log is append-only: % is not permitted', TG_OP;
        END;
     $$ LANGUAGE plpgsql",
    "CREATE TRIGGER trg_pricing_audit_log_append_only BEFORE DELETE OR UPDATE ON bss.pricing_audit_log FOR EACH ROW EXECUTE FUNCTION bss.pricing_audit_log_append_only()",
];

const PG_DOWN_STATEMENTS: &[&str] = &[
    "DROP TABLE IF EXISTS bss.pricing_audit_log",
    "DROP FUNCTION IF EXISTS bss.pricing_audit_log_append_only()",
];

const SQLITE_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE pricing_audit_log (
            tenant_id          text   NOT NULL,
            chain_id           text   NOT NULL,
            seq                bigint NOT NULL,
            action             text   NOT NULL,
            actor_principal_id text   NOT NULL,
            after_state        text,
            approval_ref       text,
            before_state       text,
            correlation_id     text,
            entry_kind         text   NOT NULL DEFAULT 'mutation',
            prev_hash          blob,
            recorded_at        text   NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00'),
            row_hash           blob   NOT NULL,
            segment_heads      text,
            subject_kind       text   NOT NULL,
            subject_ref        text   NOT NULL,
            PRIMARY KEY (tenant_id, chain_id, seq),
            CONSTRAINT chk_pricing_audit_log_action CHECK (action IN ('create','update','delete','abandon','publish','submit','approve','reject','withdraw','deny','retire','migrate')),
            CONSTRAINT chk_pricing_audit_log_entry_kind CHECK (entry_kind IN ('mutation','rollup')),
            CONSTRAINT chk_pricing_audit_log_rollup CHECK ((entry_kind = 'rollup') = (segment_heads IS NOT NULL)),
            CONSTRAINT chk_pricing_audit_log_seq CHECK (seq >= 0),
            CONSTRAINT chk_pricing_audit_log_subject_kind CHECK (subject_kind IN ('plan_revision','price_unit','window','policy','overlay','bulk_operation','membership','taxonomy_value'))
        )",
    "CREATE INDEX idx_pricing_audit_log_recorded ON pricing_audit_log (tenant_id, recorded_at)",
    "CREATE INDEX idx_pricing_audit_log_subject ON pricing_audit_log (tenant_id, subject_kind, subject_ref, recorded_at)",
    "CREATE TRIGGER trg_pricing_audit_log_no_delete BEFORE DELETE ON pricing_audit_log FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_audit_log is append-only: DELETE is not permitted'); END",
    "CREATE TRIGGER trg_pricing_audit_log_no_update BEFORE UPDATE ON pricing_audit_log FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_audit_log is append-only: UPDATE is not permitted'); END",
];

const SQLITE_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS pricing_audit_log"];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        super::exec_backend(self.name(), manager, PG_UP_STATEMENTS, SQLITE_UP_STATEMENTS).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        super::exec_backend(
            self.name(),
            manager,
            PG_DOWN_STATEMENTS,
            SQLITE_DOWN_STATEMENTS,
        )
        .await
    }
}
