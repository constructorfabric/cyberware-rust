//! Create `bss.pricing_group_membership` — the effective-dated, audited
//! membership record on `payerTenantId` (`design/09-price-overlays.md` §3
//! `inst-cg-record` / `inst-cg-resolve`, §6).
//!
//! # D-09 in two layers, not one
//!
//! §3's normative sentence is not "one group at a time": *"membership
//! intervals are **non-overlapping per payer across all groups at any
//! instant**"* — an enrollment into `groupB` while `groupA` is still active for
//! the same payer is exactly the case this table refuses, not merely the
//! narrower same-group case `MEMBERSHIP_OVERLAP` names. `window_repo`'s module
//! doc records that this crate's nearest sibling invariant — price-window
//! non-overlap — lives at a **single** layer (the repository), because the
//! window's collision domain is the **canonical scope key**, which lives on
//! `pricing_price` and not on the window row itself: no `UNIQUE`, no partial
//! index and no exclusion constraint can see a sibling row's parent's columns.
//!
//! That obstruction does not exist here. A membership's collision domain is
//! `(tenant_id, payer_tenant_id)` — both columns of **this** row — so the
//! declarative form `window_repo` documents as unavailable to it is available
//! to this table, and the repository-only arrangement is not repeated: this
//! migration carries the constraint at the schema layer, on both engines,
//! rather than leaving the interval free to collide until a Rust read notices.
//!
//! # Postgres: `EXCLUDE USING gist`, scoped by equality on tenant and payer,
//! never on `group_value`
//!
//! ```sql
//! EXCLUDE USING gist (
//!     tenant_id WITH =,
//!     payer_tenant_id WITH =,
//!     tstzrange(effective_from, effective_to, '[)') WITH &&)
//! ```
//!
//! `group_value` is deliberately **absent** from the equality list. Scoping by
//! it would only refuse a same-group collision — `MEMBERSHIP_OVERLAP` — and
//! admit the cross-group one D-09 is actually about, which is the false-negative
//! shape this migration's brief calls out by name. Scoped by tenant and payer
//! alone, the one exclusion constraint refuses both: a second interval in the
//! same group collides with itself under `&&`, and a second interval in a
//! *different* group collides too, because the group column never entered the
//! equality set that would have separated them.
//!
//! `tstzrange(effective_from, effective_to, '[)')` with a `NULL` upper bound is
//! Postgres's own open-ended range — no sentinel timestamp is needed — and the
//! `[)` bound spec is what makes `effective_to = next.effective_from` adjacency
//! rather than a collision, the same half-open reading `window_repo` documents
//! for its own interval.
//!
//! `EXCLUDE` needs `btree_gist` for the equality operators over `uuid`
//! (`tstzrange` already has native `gist` support). The extension is available in
//! this deployment's Postgres image (`postgres:16-alpine`, the tag every suite in
//! this crate pins, carries it as a contrib module) and it is created by
//! `m20260821_000002_install_btree_gist`, ahead of every table. Owning it here
//! instead would make this migration's `down` drop the floor out from under
//! `pricing_price_window`'s exclusion constraint.
//!
//! # `SQLite`: no exclusion constraint exists, so the equivalent is a pair of
//! `RAISE(ABORT, …)` triggers
//!
//! One `BEFORE INSERT` and one `BEFORE UPDATE`, both spelling the same
//! NULL-safe half-open overlap test `window_repo::intersects` documents in
//! Rust — `a.from < b.to_or_infinity && b.from < a.to_or_infinity`, written here
//! as `(existing.effective_to IS NULL OR NEW.effective_from < existing.effective_to)
//! AND (NEW.effective_to IS NULL OR existing.effective_from < NEW.effective_to)`
//! — over **no sentinel**: a `NULL` bound reads as "no bound" directly rather
//! than through a synthesized far-future timestamp, which would only be as
//! correct as its format agreeing with whatever `SeaORM` happens to serialize a
//! `OffsetDateTime` as as text.
//!
//! Neither trigger's guard lives in its `WHEN` clause: `pricing_bulk_row_lock`'s
//! module doc records that a `SQLite` trigger's `WHEN` may not contain a
//! subquery, so — as that migration and `pricing_composite_meter` both do — the cross-row
//! `EXISTS` lives in the trigger **body**, as `SELECT RAISE(ABORT, …) WHERE
//! EXISTS (…)`, which fires exactly when the subquery finds a colliding row and
//! is silent otherwise.
//!
//! The `UPDATE` trigger excludes the row's own previous self
//! (`existing.membership_id <> NEW.membership_id`) so that ending a membership
//! early — `inst-ms-time`'s "ending early = setting `to`" — does not collide
//! with the very row being shortened; the `INSERT` trigger carries no such
//! exclusion because the new row has no prior self to exclude.
//!
//! # Two spellings, and why `tests/postgres_group_membership.rs` holds them equal
//!
//! `tests/postgres_clone_atomicity.rs:4-14` states the reason this crate writes
//! every guard twice rather than once: two spellings of one rule are only ever
//! as equal as the suite that exercises both. Here that suite proves the
//! cross-group case D-09 is actually about — two different `group_value`s, one
//! payer, overlapping intervals — refused **by the database** on Postgres, and
//! `tests/sqlite_migrations.rs`'s trigger and CHECK censuses hold the `SQLite`
//! mirror to the same shape structurally; the behavioural proof of the `SQLite`
//! arm belongs to the fast in-crate suite the same way `window_repo`'s does.
//!
//! # What is deliberately not here
//!
//! No `state` column and no state-machine machinery: §4's three states
//! (`scheduled` / `active` / `ended`) are computed from `now()` against
//! `[effective_from, effective_to)`, not stored (`cpt-cf-bss-pricing-state-membership`).
//! No `reason` / `actor` columns either — §6 lists them as "audit surface (full
//! trail in `pricing_audit_log`)", and `pricing_audit_log` (`pricing_audit_log`)
//! is where the mutation's actor, instant and before/after already live; a
//! second copy on this row would be a second thing to keep true, `pricing_price_overlay`'s
//! argument for the same omission on `pricing_price_overlay`. No append-only /
//! frozen-column trigger pair either: unlike the revision-chain tables, a
//! membership row's own columns (other than the key) are exactly what an
//! authorized `PATCH` — ending or adjusting an interval — is meant to move, and
//! that route's write path (not built by this migration) is where a
//! precondition on `row_version` belongs.
//!
//! **The repository-level second check `design/09-price-overlays.md` implies is
//! not built here.** This migration is the storage layer only — no
//! `membership_repo`, no domain type, no route exists yet for it to sit inside
//! — and is reported as owed rather than silently assumed away; see this
//! migration's own task report.
//!
//! **Backend differences.** As `pricing_price_overlay`: `bss.`-qualified DDL and a
//! `uuid`/`timestamptz` column set on Postgres; unqualified, `text`-typed and
//! trigger-guarded on `SQLite`.
//!
//! # What `idx_pricing_group_membership_walk` is for
//!
//! `(tenant_id, group_value, effective_from, membership_id)` — the filter's two
//! columns followed by the walk's sort key **in the order the walk sorts**. The pair
//! at the end matters: the cursor resumes on `(effective_from, membership_id)`, so an
//! index stopping at `effective_from` would still sort the tail of every
//! equal-instant run.
//!
//! It reached the Postgres server late, and not by anyone reading the migrations —
//! `postgres_migrations`' index census compares the server's roster against
//! `EXPECTED_INDEXES`, one roster **per engine**, and a shared list would have been
//! satisfied by the `SQLite` half while the engine that ships was missing it.
//!
//! # `group_value` is held to the taxonomies' blankness predicate
//!
//! `chk_pricing_group_membership_group_value_present` strips ASCII whitespace entire
//! — tab, line feed, vertical tab, form feed, carriage return, space — and refuses a
//! value left with nothing. The rule and its argument are
//! `pricing_region_taxonomy`'s (D-242), including the character set, why the two
//! engines spell it two ways, and the non-ASCII whitespace residue no `CHECK` on this
//! chain reaches.
//!
//! `length(group_value) > 0` is the wrong predicate for this column and is the
//! loosest form the chain can carry: a **single space** satisfies it. The group value
//! is the name `inst-cg-resolve` resolves a payer's price by, so a blank one is a
//! group `required_group` cannot address — it mints the path segment through
//! `ScopeValue::new`, which trims — and that nothing can tell from another blank.
//! The field is a plain `String` on `MembershipMoveProposal`, so the trim lives at the REST
//! door rather than in the type, and this predicate is what the door's absence would
//! leave.
//!
//! Dependency level 0.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const PG_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE bss.pricing_group_membership (
            tenant_id       uuid        NOT NULL,
            membership_id   uuid        NOT NULL,
            effective_from  timestamptz NOT NULL,
            effective_to    timestamptz,
            group_value     text        NOT NULL,
            payer_tenant_id uuid        NOT NULL,
            created_at_utc  timestamptz NOT NULL DEFAULT now(),
            created_by      uuid        NOT NULL,
            row_version     bigint      NOT NULL DEFAULT 0,
            CONSTRAINT chk_pricing_group_membership_group_value_present CHECK (length(btrim(group_value, chr(9) || chr(10) || chr(11) || chr(12) || chr(13) || chr(32))) > 0),
            CONSTRAINT chk_pricing_group_membership_interval CHECK (effective_to IS NULL OR effective_to > effective_from),
            CONSTRAINT chk_pricing_group_membership_row_version CHECK (row_version >= 0),
            CONSTRAINT excl_pricing_group_membership_no_overlap EXCLUDE USING gist (tenant_id WITH =, payer_tenant_id WITH =, tstzrange(effective_from, effective_to, '[)'::text) WITH &&),
            CONSTRAINT pricing_group_membership_pkey PRIMARY KEY (membership_id)
        )",
    "CREATE INDEX idx_pricing_group_membership_payer ON bss.pricing_group_membership USING btree (tenant_id, payer_tenant_id, effective_from)",
    "CREATE INDEX idx_pricing_group_membership_walk ON bss.pricing_group_membership USING btree (tenant_id, group_value, effective_from, membership_id)",
];

const PG_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS bss.pricing_group_membership"];

const SQLITE_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE pricing_group_membership (
            tenant_id       text   NOT NULL,
            membership_id   text   NOT NULL,
            effective_from  text   NOT NULL,
            effective_to    text,
            group_value     text   NOT NULL,
            payer_tenant_id text   NOT NULL,
            created_at_utc  text   NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00'),
            created_by      text   NOT NULL,
            row_version     bigint NOT NULL DEFAULT 0,
            PRIMARY KEY (membership_id),
            CONSTRAINT chk_pricing_group_membership_group_value_present CHECK (length(trim(group_value, char(9,10,11,12,13,32))) > 0),
            CONSTRAINT chk_pricing_group_membership_interval CHECK (effective_to IS NULL OR effective_to > effective_from),
            CONSTRAINT chk_pricing_group_membership_row_version CHECK (row_version >= 0)
        )",
    "CREATE INDEX idx_pricing_group_membership_payer ON pricing_group_membership (tenant_id, payer_tenant_id, effective_from)",
    "CREATE INDEX idx_pricing_group_membership_walk ON pricing_group_membership (tenant_id, group_value, effective_from, membership_id)",
    "CREATE TRIGGER trg_pricing_group_membership_no_overlap_insert BEFORE INSERT ON pricing_group_membership FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_group_membership: interval overlaps an existing membership for this payer (D-09)') WHERE EXISTS (SELECT 1 FROM pricing_group_membership existing WHERE existing.tenant_id = NEW.tenant_id AND existing.payer_tenant_id = NEW.payer_tenant_id AND (existing.effective_to IS NULL OR NEW.effective_from < existing.effective_to) AND (NEW.effective_to IS NULL OR existing.effective_from < NEW.effective_to)); END",
    "CREATE TRIGGER trg_pricing_group_membership_no_overlap_update BEFORE UPDATE ON pricing_group_membership FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_group_membership: interval overlaps an existing membership for this payer (D-09)') WHERE EXISTS (SELECT 1 FROM pricing_group_membership existing WHERE existing.tenant_id = NEW.tenant_id AND existing.payer_tenant_id = NEW.payer_tenant_id AND existing.membership_id <> NEW.membership_id AND (existing.effective_to IS NULL OR NEW.effective_from < existing.effective_to) AND (NEW.effective_to IS NULL OR existing.effective_from < NEW.effective_to)); END",
];

const SQLITE_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS pricing_group_membership"];

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
