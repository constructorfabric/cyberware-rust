//! Create `bss.pricing_plan_phase` — the phase chain of **one plan revision**
//! (`design/02-plan-definition.md` §6), keyed
//! `(tenant_id, plan_id, plan_revision, phase_id)`. The
//! first of Slice 2's three revision-scoped child tables, and the one the
//! canonical scope key points at.
//!
//! **Two halves of the key are the phase's own and two are its scope.**
//! `plan_revision` is
//! there because a plan's shape versions with its revision: a new revision
//! **copies** the phase rows under its own number and the open draft edits its
//! own copies (D-83). `phase_id` is there unchanged, and its stability is the
//! whole point — the `phase` axis of the canonical scope key holds a **bare**
//! `phase_id` (D-19), and same-key supersession compares it (D-56). Re-minting
//! an id on a new revision would move every continuing price row onto a key
//! nothing else is filed under: the rows would stop superseding their
//! predecessors, phase coverage would judge a chain nobody authored, and the
//! damage would first be visible as a rating miss on a plan that published
//! cleanly.
//!
//! **`tenant_id` is not in §6's column list, and is here anyway.** §6's own
//! preamble calls these tables "tenant-scoped, `SecureORM`" and
//! `01-foundation.md` §3.7 says it of every physical table in this gear;
//! `Scopable` has nowhere else to read the tenant from. The omission is
//! reported rather than treated as a decision. The value is copied from the
//! parent revision by the repository and never taken from a request (Global
//! Constraint 9): the foreign key covers the plan key alone, so nothing here
//! stops a child carrying a different tenant from the row it points at, and
//! under `SecureORM` such a child is invisible to its true owner while still
//! joined by key to their plan.
//!
//! # At most one terminal phase, and the other half of the rule is not here
//!
//! `uq_pricing_plan_phase_terminal` is a partial `UNIQUE` over
//! `(plan_id, plan_revision) WHERE converts_to_phase_id IS NULL`: a revision
//! may not hold two phases with no successor. §6 is explicit that the
//! **existence** of exactly one terminal phase is the `PhaseGraph` pipeline's
//! rule (`inst-ph-graph`, `PHASE_GRAPH_INVALID`) — an index cannot enforce the
//! `>= 1` half, because a row is what an index sees and "there is no such row"
//! is not a row. Nobody should later try to "strengthen" this index into the
//! whole rule; what they would get is a weaker rule wearing the name of the
//! real one, and the zero-terminal chain — the one that leaves every phase
//! conversion resolving to nothing — would still publish.
//!
//! Wholesale replacement of a revision's phase set (`PlanShapeRepo`) is what
//! makes this index authorable at all: re-ordering a chain by moving one row at
//! a time transiently holds two terminals, and the index would refuse an edit
//! whose end state is legal.
//!
//! # Three constraints this table deliberately does NOT carry
//!
//! Each is a real rule, and each belongs to the validation pipeline, which
//! judges a shape the author assembled and owes them **one report enumerating
//! every finding**. A constraint here would answer with a driver error instead
//! — an internal fault where a line an operator can act on was owed — and would
//! refuse a half-authored draft besides, since a phase graph is authored in
//! successive `PATCH`es rather than all at once.
//!
//! 1. **No `CHECK` pairing `phase_duration_days` with terminality.** §6's "Key
//!    constraints" paragraph lists none, and `inst-ph-duration` assigns the
//!    rule — required `> 0` on every non-terminal phase, forbidden on the
//!    terminal one — to the pipeline as `PHASE_DURATION_INVALID`. An author who
//!    sets a successor in one request and its duration in the next passes
//!    through the state such a CHECK would forbid.
//! 2. **No `CHECK` that a terminal phase's `kind` is `evergreen`.** C-4 gives
//!    that rule its own code, `TERMINAL_PHASE_KIND_INVALID`, for the same
//!    authoring reason: terminality is structural
//!    (`converts_to_phase_id IS NULL`) and the `kind` column is independent of
//!    it, which is exactly the pair the rule exists to reject.
//! 3. **No foreign key on `converts_to_phase_id`.** "No dangling target" is
//!    `inst-ph-graph`'s rule. A self-referential FK would forbid authoring
//!    phase A's successor before phase B exists — the ordinary way a chain is
//!    written — and would answer a genuinely dangling edge with a constraint
//!    violation rather than a report line naming the phase.
//!
//! A **floor** is none of those three rules, and it does sit here.
//! `chk_pricing_plan_phase_duration_non_negative` and
//! `chk_pricing_plan_phase_trial_projection_non_negative` refuse a negative day
//! count and nothing else: neither requires a duration nor forbids one, so the
//! half-authored draft point 1 protects still saves. What they close is the
//! poisoned row. Both columns are read back through `u32::try_from` and a
//! negative answers `CorruptRow`, which is an internal fault on **every** later
//! read of the revision — past the point where the pipeline's report could be
//! reached at all, and unreachable by any correction this gear offers.
//!
//! # `display_trial_days` may not drift from its source
//!
//! `chk_pricing_plan_phase_display_trial_days` is §6's, verbatim: a `trial`
//! phase publishes `displayTrialDays` as the PRD-named projection of its
//! `phaseDurationDays` (`inst-ph-trial`), one value under two persisted names,
//! and the two may never disagree (2026-07-28 review fix). Subscriptions reads
//! the published projection as its single source for trial runtime, so a drift
//! here is a trial that ends on a different day than the catalog says it does.
//!
//! The CHECK is satisfied when `display_trial_days` is NULL — an untaken
//! projection — and, by SQL's NULL propagation, **also when
//! `phase_duration_days` is NULL while `display_trial_days` is set**: the
//! comparison is then NULL, which both engines count as satisfied. That
//! remaining shape is a trial phase projecting a duration it does not have, and
//! it is refused at publish rather than here, by the two rules already named:
//! a non-terminal phase without a duration is `PHASE_DURATION_INVALID`, and a
//! terminal one carrying the `trial` kind is `TERMINAL_PHASE_KIND_INVALID`.
//! Closing it with an extra `phase_duration_days IS NOT NULL` conjunct would
//! make exactly the half-authored draft the paragraph above protects unsavable,
//! so the schema stands behind the pipeline here rather than in front of it.
//!
//! # Append-only with its revision (`01-foundation.md` §3.7, the L-2 fix)
//!
//! Every revision-scoped child table carries the same discipline as its parent:
//! child rows are physically immutable once **their** revision publishes, while
//! a draft revision's copies stay freely mutable and deletable. There is no
//! `lifecycle_state` on this table — the parent revision's is the referent —
//! so the predicate reads `pricing_plan.lifecycle_state = 'draft'` for
//! `(plan_id, plan_revision)`, exactly as `pricing_price_tier_band` reads its
//! parent price row. Without it, the phase set of a frozen revision could be
//! rewritten under an unchanged `pricing_plan`, and the projector's warm
//! re-drive — which reads truth rows (§4.4) — would quietly re-materialize a
//! frozen `CatalogVersion` at a different shape.
//!
//! INSERT is guarded and not only UPDATE and DELETE, because an INSERT is the
//! one verb that **adds** a phase to a frozen revision. An UPDATE is checked
//! against **both** ends: the `OLD` parent, the revision the row is bound to
//! now, whose freeze governs it; and the `NEW` parent, the revision the row
//! would land under, whose freeze forbids the append. That is not
//! belt-and-braces — re-pointing a child row's `plan_revision` is precisely how
//! one would otherwise append to a frozen revision without ever issuing an
//! INSERT.
//!
//! **`abandoned` is not `draft`**, and the ordering that follows from it is
//! mandatory rather than stylistic. D-145 drops a discarded revision's child
//! copies; the moment the revision row reads `abandoned` this trigger refuses
//! the DELETE, so `PlanRepo::abandon_draft` drops the phase rows **before** it
//! flips the revision, in one transaction. The mirror-image ordering holds on
//! the way in: `PlanRepo::open_revision` inserts the new revision row first,
//! because the INSERT arm reads the *new* parent and requires it to be `draft`.
//!
//! **Backend differences.** Postgres carries the rule as one PL/pgSQL trigger
//! function with the offending state interpolated; `SQLite` has no procedural
//! language and `RAISE(ABORT, ...)` takes a literal message, so the same rule
//! becomes three fixed-message triggers, one per DML verb, whose parent lookup
//! is a `WHERE NOT EXISTS` subquery in the trigger **body**.
//!
//! The body rather than a `WHEN` clause is this chain's spelling for a guard that
//! reads another row, and **not** an engine limitation: a `SQLite` `WHEN` does
//! accept a subquery — `pricing_approval_key`'s triggers put a scalar `SELECT` in
//! four of theirs and they enforce, and sqlite 3.51 admits `WHEN NOT EXISTS (…)`
//! directly. The
//! body form is taken so each arm's condition sits beside the message it raises,
//! one trigger per arm of the PL/pgSQL function.
//!
//! `uuid` becomes `text` and the `bss.` qualification is dropped, as elsewhere in
//! this chain. Every CHECK, index, FK and PK is preserved on both sides.
//!
//! # Why the scope is in the key, and what the key still refuses
//!
//! `tenant_id` and `plan_id` are in the primary key by D-340, and without them the
//! key asserts something nobody ever argued: that a phase id belongs to **one plan
//! per revision number across the whole table**, every tenant's included. Two
//! consequences, and the second is the serious one.
//!
//! An operator seeding several drafts with one phase id gets exactly one plan that
//! can attach it; the rest collide at every revision number they will ever hold, and
//! since a scope key **is** a price row's identity (`01-foundation.md` §3.7) those
//! rows cannot be re-pointed at a different phase either — leaving deletion as the
//! only remedy. Measured on the stand 2026-08-17: five drafts, one shared phase id,
//! four of them unrepairable through the API.
//!
//! And the reach is every `plan × write` holder, not a script's. The `phases` PATCH
//! facet takes `phase_id` from the client — which is what makes D-19's clone remap
//! and D-83's copy-forward expressible at all — so the difference between a `500`
//! and a `200` answers *is this id in use somewhere I cannot read*: a probe of
//! another tenant's phase ids, on a table this gear scopes by `tenant_id`
//! everywhere else.
//!
//! **What the key still refuses is not a leftover.**
//! `(tenant_id, plan_id, plan_revision, phase_id)` admits exactly one row per
//! `(plan, revision, phase)`, so one revision may still not hold the same phase id
//! twice. `inst-ph-graph` walks a chain and `inst-ph-default` speaks of *the*
//! terminal phase, and both are written as though a phase id names at most one row
//! of a revision. A key that also admitted the duplicate would fix the collision
//! above and quietly make the graph rules judge a chain nobody can author. The
//! paired probes in `tests/sqlite_plan_phase.rs` are one per direction for that
//! reason.
//!
//! `uq_pricing_plan_phase_terminal` is partial over
//! `(plan_id, plan_revision) WHERE converts_to_phase_id IS NULL` — per plan, not per
//! id — so "at most one terminal phase" was never carried by the primary key and
//! does not depend on its shape. No foreign key in this schema names
//! `pricing_plan_phase`, and `plan_phase::Entity::find_by_id` has no call sites, so
//! the key's arity is not a signature anything spells out.
//!
//! Dependency level 1.
//!
//! # `display_name`, and the kind roster (D-357, D-358)
//!
//! `display_name text` is the operator's optional label for the phase (D-357):
//! free-form, no uniqueness, no behaviour, versioned with the revision like every
//! other column. The `kind` roster is `trial | interim | evergreen` — the middle
//! token was `intro` until D-358, renamed because it named a business meaning
//! nothing behaved on; the label above is where that meaning now lives. **Edited
//! in place** (this chain's convention), which means a stand that already applied
//! this migration keeps the old DDL until the equivalent `ALTER` is run — D-358
//! carries the runbook. Frozen `CatalogVersion` snapshots taken before the rename
//! keep `"kind": "intro"`; nothing in this gear parses `kind` back out of one.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const PG_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE bss.pricing_plan_phase (
            tenant_id            uuid    NOT NULL,
            plan_id              uuid    NOT NULL,
            plan_revision        bigint  NOT NULL,
            phase_id             uuid    NOT NULL,
            converts_to_phase_id uuid,
            display_trial_days   integer,
            kind                 text    NOT NULL,
            display_name         text,
            ordinal              integer NOT NULL,
            phase_duration_days  integer,
            CONSTRAINT chk_pricing_plan_phase_display_trial_days CHECK (display_trial_days IS NULL OR display_trial_days = phase_duration_days),
            CONSTRAINT chk_pricing_plan_phase_duration_non_negative CHECK (phase_duration_days IS NULL OR phase_duration_days >= 0),
            CONSTRAINT chk_pricing_plan_phase_trial_projection_non_negative CHECK (display_trial_days IS NULL OR display_trial_days >= 0),
            CONSTRAINT chk_pricing_plan_phase_kind CHECK (kind IN ('trial','interim','evergreen')),
            CONSTRAINT fk_pricing_plan_phase_revision FOREIGN KEY (plan_id, plan_revision) REFERENCES bss.pricing_plan(plan_id, revision),
            CONSTRAINT pricing_plan_phase_pkey PRIMARY KEY (tenant_id, plan_id, plan_revision, phase_id)
        )",
    "CREATE INDEX idx_pricing_plan_phase_revision ON bss.pricing_plan_phase USING btree (tenant_id, plan_id, plan_revision)",
    "CREATE UNIQUE INDEX uq_pricing_plan_phase_terminal ON bss.pricing_plan_phase USING btree (plan_id, plan_revision) WHERE (converts_to_phase_id IS NULL)",
    "CREATE OR REPLACE FUNCTION bss.pricing_plan_phase_append_only() RETURNS trigger AS $$
        DECLARE
          parent_state  text;
          parent_tenant uuid;
        BEGIN
          IF TG_OP <> 'INSERT' THEN
            SELECT lifecycle_state INTO parent_state
              FROM bss.pricing_plan
             WHERE plan_id = OLD.plan_id AND revision = OLD.plan_revision;
            IF parent_state IS DISTINCT FROM 'draft' THEN
              RAISE EXCEPTION
                'pricing_plan_phase: % of a phase under a % plan revision is not permitted',
                TG_OP, coalesce(parent_state, 'missing');
            END IF;
          END IF;

          IF TG_OP = 'DELETE' THEN
            RETURN OLD;
          END IF;

          SELECT lifecycle_state, tenant_id INTO parent_state, parent_tenant
            FROM bss.pricing_plan
           WHERE plan_id = NEW.plan_id AND revision = NEW.plan_revision;
          IF parent_state IS DISTINCT FROM 'draft' THEN
            RAISE EXCEPTION
              'pricing_plan_phase: % of a phase under a % plan revision is not permitted',
              TG_OP, coalesce(parent_state, 'missing');
          END IF;

          -- `fk_pricing_plan_phase_revision` covers `(plan_id, plan_revision)` alone, so
          -- without this arm a row could carry a tenant its own parent revision
          -- does not belong to: invisible to every scoped reader, and frozen with
          -- the revision it was written under. The state arm above has already
          -- refused a parent that does not exist, so a foreign tenant is the only
          -- thing left for this one to find.
          IF parent_tenant IS DISTINCT FROM NEW.tenant_id THEN
            RAISE EXCEPTION
              'pricing_plan_phase: plan revision %/% belongs to another tenant and may not hold this row',
              NEW.plan_id, NEW.plan_revision;
          END IF;

          RETURN NEW;
        END;
     $$ LANGUAGE plpgsql",
    "CREATE TRIGGER trg_pricing_plan_phase_append_only BEFORE INSERT OR DELETE OR UPDATE ON bss.pricing_plan_phase FOR EACH ROW EXECUTE FUNCTION bss.pricing_plan_phase_append_only()",
];

const PG_DOWN_STATEMENTS: &[&str] = &[
    "DROP TABLE IF EXISTS bss.pricing_plan_phase",
    "DROP FUNCTION IF EXISTS bss.pricing_plan_phase_append_only()",
];

const SQLITE_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE pricing_plan_phase (
            tenant_id            text   NOT NULL,
            plan_id              text   NOT NULL,
            plan_revision        bigint NOT NULL,
            phase_id             text   NOT NULL,
            converts_to_phase_id text,
            display_trial_days   int,
            kind                 text   NOT NULL,
            display_name         text,
            ordinal              int    NOT NULL,
            phase_duration_days  int,
            PRIMARY KEY (tenant_id, plan_id, plan_revision, phase_id),
            CONSTRAINT chk_pricing_plan_phase_display_trial_days CHECK (display_trial_days IS NULL OR display_trial_days = phase_duration_days),
            CONSTRAINT chk_pricing_plan_phase_duration_non_negative CHECK (phase_duration_days IS NULL OR phase_duration_days >= 0),
            CONSTRAINT chk_pricing_plan_phase_trial_projection_non_negative CHECK (display_trial_days IS NULL OR display_trial_days >= 0),
            CONSTRAINT chk_pricing_plan_phase_kind CHECK (kind IN ('trial','interim','evergreen')),
            CONSTRAINT fk_pricing_plan_phase_revision FOREIGN KEY (plan_id, plan_revision) REFERENCES pricing_plan(plan_id, revision)
        )",
    "CREATE INDEX idx_pricing_plan_phase_revision ON pricing_plan_phase (tenant_id, plan_id, plan_revision)",
    "CREATE UNIQUE INDEX uq_pricing_plan_phase_terminal ON pricing_plan_phase (plan_id, plan_revision) WHERE converts_to_phase_id IS NULL",
    "CREATE TRIGGER trg_pricing_plan_phase_no_delete BEFORE DELETE ON pricing_plan_phase FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_plan_phase: DELETE of a phase under a non-draft plan revision is not permitted') WHERE NOT EXISTS (SELECT 1 FROM pricing_plan WHERE plan_id = OLD.plan_id AND revision = OLD.plan_revision AND lifecycle_state = 'draft'); END",
    "CREATE TRIGGER trg_pricing_plan_phase_no_insert BEFORE INSERT ON pricing_plan_phase FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_plan_phase: INSERT of a phase under a non-draft plan revision is not permitted') WHERE NOT EXISTS (SELECT 1 FROM pricing_plan WHERE plan_id = NEW.plan_id AND revision = NEW.plan_revision AND lifecycle_state = 'draft'); END",
    "CREATE TRIGGER trg_pricing_plan_phase_no_update BEFORE UPDATE ON pricing_plan_phase FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_plan_phase: UPDATE of a phase under a non-draft plan revision is not permitted') WHERE NOT EXISTS (SELECT 1 FROM pricing_plan WHERE plan_id = OLD.plan_id AND revision = OLD.plan_revision AND lifecycle_state = 'draft') OR NOT EXISTS (SELECT 1 FROM pricing_plan WHERE plan_id = NEW.plan_id AND revision = NEW.plan_revision AND lifecycle_state = 'draft'); END",
    "CREATE TRIGGER trg_pricing_plan_phase_same_tenant_as_its_revision_on_insert BEFORE INSERT ON pricing_plan_phase FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_plan_phase: the plan revision belongs to another tenant and may not hold this row') WHERE EXISTS (SELECT 1 FROM pricing_plan WHERE plan_id = NEW.plan_id AND revision = NEW.plan_revision) AND NOT EXISTS (SELECT 1 FROM pricing_plan WHERE plan_id = NEW.plan_id AND revision = NEW.plan_revision AND tenant_id = NEW.tenant_id); END",
    "CREATE TRIGGER trg_pricing_plan_phase_same_tenant_as_its_revision_on_update BEFORE UPDATE ON pricing_plan_phase FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_plan_phase: the plan revision belongs to another tenant and may not hold this row') WHERE EXISTS (SELECT 1 FROM pricing_plan WHERE plan_id = NEW.plan_id AND revision = NEW.plan_revision) AND NOT EXISTS (SELECT 1 FROM pricing_plan WHERE plan_id = NEW.plan_id AND revision = NEW.plan_revision AND tenant_id = NEW.tenant_id); END",
];

const SQLITE_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS pricing_plan_phase"];

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
