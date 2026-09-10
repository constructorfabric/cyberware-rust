//! Create `bss.pricing_approval` — the approval workflow's record
//! (`design/05-governance.md` §6, `cpt-cf-bss-pricing-state-approval`), the
//! store the two-person rule is decided in and the only thing that can hand
//! `PublishAuthorization::Approved` to a publish commit.
//!
//! The state machine of §4 lives in the constraints, not only in the domain:
//! `submitted -> approved | rejected | voided` and nothing else
//! (`inst-as-approve`, `inst-as-reject`, `inst-as-void`), a decided record
//! frozen forever (`inst-as-immutable`), a reject carrying its mandatory reason,
//! and an approver who is never the submitter (`inst-tp-distinct`). Each of
//! those is a rule the domain also states, and each is here for the reason
//! `pricing_plan`'s whitelist is: a rule that lives only in application code is
//! one ad-hoc `UPDATE` away from being bypassed, and what it would be bypassing
//! here is the evidence that a human other than the author agreed to the price
//! change.
//!
//! # `subject_kind` declares a **subset** of S5 §6's ten, and the CHECK is the roster
//!
//! D-158 requires `pricing_approval` and `pricing_audit_log` to spell **one**
//! enumeration and to be extended together, so that the approval record and the
//! audit record of one decision cannot disagree about what the decision was
//! about. What is declared is therefore whatever `AuditSubjectKind` declares, under
//! the same section's standing rule that a token with no writer is not declared —
//! and **the CHECK below is the roster; this paragraph deliberately does not repeat
//! its members.** It used to, and the count went stale the day `window` arrived:
//! a count in prose beside a roster in code leaves only one of the two true, and it
//! is never the prose. `AuditSubjectKind::ALL` and
//! `tests/sqlite_approval_repo.rs`'s
//! `every_subject_kind_d158_declares_is_storable_on_the_mirror` are what hold the
//! two sides equal, over `ALL` rather than over a written-out list.
//!
//! Declaring the members that have no writer would break D-158 in the direction it
//! exists to prevent — and would read as coverage to everyone who greps for it,
//! since nothing in this gear can open an approval over an overlay, a membership, a
//! bundle, a retirement, a policy, a historical import or a bulk batch.
//!
//! The `CHECK` is what makes that a declaration rather than a comment.
//! `pricing_audit_log` types its own `subject_kind` as free `text` (the column
//! predates any declared vocabulary); this table does not repeat that, because
//! S5 §6 types the column `enum` and the gear already spells the same
//! discriminator with a `CHECK` on `pricing_read_model` and
//! `pricing_catalog_version_ref`.
//!
//! # Two divergences from S5 §6's literal column notes, both reported
//!
//! **`approver_principal <> submitter_principal` needs an `IS NULL` arm.** The
//! table's note gives the bare comparison, and the same row says
//! `approver_principal` is "NULL until decided". A bare `<>` is NULL — hence
//! *satisfied* — on both engines when either side is NULL, so the literal form
//! happens to work; but writing it bare states an invariant the column's own
//! nullability contradicts, and a later reader tightening it to
//! `IS DISTINCT FROM` (the spelling that means what the sentence says) would
//! make **every open record unstorable** at a stroke. The arm is spelled out so
//! that reading is unavailable.
//!
//! **`subject_ref` is `text` and the principals are `uuid`.** S5 §6 types
//! `subject_ref` as `uuid` and both principals as `string`; this table inverts
//! both. A plan revision's durable name is `(plan_id, revision)` — rendered
//! `<plan_id>/<revision>` by `audit_repo::plan_revision_ref` and stored as
//! `text` in `pricing_audit_log.subject_ref` — so a `uuid` column could not hold
//! the one subject this phase writes, and D-158's "same enumeration" would be
//! paired with two incompatible reference types. The principals go the other
//! way for the mirror-image reason: `pricing_audit_log.actor_principal_id` is
//! `uuid`, and the two-person rule compares an approver against a submitter
//! whose identity the audit trail already holds in that type. Both are reported
//! rather than reconciled by editing the design set.
//!
//! # The trigger is the whitelist shape, and what it pins
//!
//! **A record is born `submitted`.** `INSERT` of any other state is refused
//! outright — §4 names `submitted` as the machine's initial state, and every
//! other rule below is written about a row that started there. Guarding `UPDATE`
//! and `DELETE` alone leaves a row free to be born `approved` with the whole
//! decision plane bypassed *because there is no `UPDATE` to bypass it on*. On a
//! table whose entire purpose is to be the evidence that a second human agreed,
//! that is the two-person rule defeated by one statement — and once publish reads
//! `PublishAuthorization` off this table, defeated silently.
//!
//! `pricing_approval_key`, `pricing_bulk_operation` and
//! `pricing_repricing_journal` carry the same three-verb guard, for the same
//! reason. The two schema goldens list every trigger the chain creates with the
//! verbs it fires on, and are where to read the whole set rather than a sample of
//! it — a neighbouring migration's file names its own trigger and says nothing
//! about whether it has one.
//!
//! `DELETE` is refused unconditionally: a decided record is the evidence, and a
//! `submitted` one is what `PENDING_CHANGE_UNIT_EXISTS` reads, so removing
//! either is removing the answer to a question rather than tidying up.
//!
//! An `UPDATE` of a record that is no longer `submitted` is refused outright —
//! `inst-as-immutable`, and a re-submit opens a **new** record. On the
//! `submitted` plane the eight non-decision columns are pinned, which is not
//! bookkeeping either: `content_hash` **is** the TOCTOU guard (`inst-ap-pin`),
//! and a hash that could be re-pinned in place would let the mutation the guard
//! exists to catch be laundered into an approval that verifies. Exactly four
//! columns may move, once: `state`, `approver_principal`, `reason`,
//! `decided_at`. Membership is tested rather than change, as
//! `pricing_plan_append_only` tests it and for the same reason — a
//! `NEW IS DISTINCT FROM OLD` conjunct would let the `SQLite` mirror accept a
//! no-op the Postgres branch refuses, and a backend divergence is worse than the
//! hole it would close.
//!
//! There is no `REVOKE`. It names a deployment role this migration does not own
//! and `SQLite` has no `GRANT`/`REVOKE` at all; the trigger is the portable half
//! of the discipline the design set calls "REVOKE + trigger"
//! (see `pricing_plan`'s module doc).
//!
//! **Backend differences.** The systematic type mirror (`uuid` -> `text`,
//! `timestamptz` -> `text`, `jsonb` -> `text`, `bytea` -> `blob`), plus the
//! trigger split: Postgres carries one PL/pgSQL function interpolating the
//! offending values, while `SQLite` has no procedural language and
//! `RAISE(ABORT, ...)` takes a **literal** message only, so the same five rules
//! become five triggers with fixed messages and `IS DISTINCT FROM` written
//! `IS NOT`. The Postgres `down` drops the function as well as the table; the
//! `SQLite` one drops only the table, there being no function to drop.
//!
//! `pricing_approval` gains `uq_pricing_approval_policy_pending` — "one open
//! policy proposal per tenant" as an **index** rather than as a read-then-write
//! check (D-192 clause (2)).
//!
//! # What was unguarded, and it is the mint
//!
//! `pricing_approval_threshold`'s primary key is `(tenant_id, version, currency)`,
//! and that is deliberate: the store permits one mutation of an approved version —
//! an `INSERT` of a currency it did not have — and relies on the content pin to take
//! a widened version *out of effect* rather than let it quietly extend what an
//! approver signed.
//! `rest_threshold_policy::a_version_widened_after_approval_stops_being_the_effective_policy`
//! is that behaviour's pin, and it is why the guard here is **not** a version-header
//! table keyed `(tenant_id, version)`: such a table would forbid the widening and
//! redden the case that pins it (D-192 clause (3), rejected option (a)).
//!
//! What is unguarded is the **mint**. `threshold_repo::open_version` does not check
//! whether a version number is already taken, and the rule that stops two proposals
//! reaching for one number — "one open policy proposal per tenant" — was a
//! read-then-write check: `infra::approval::open_policy_unit` reads
//! `approval_repo::find_pending_policy_unit` and then inserts. Under `READ
//! COMMITTED` both proposals read a store with no pending policy unit, both mint
//! version *n* off the same `latest_version`, and both commit — leaving one version
//! number holding the union of two disjoint currency sets, which is a row set no
//! approver ever saw and which the store then refuses to `UPDATE` or `DELETE`.
//!
//! # The subject is the **proposal**, not the version
//!
//! A policy proposal's open unit is a `pricing_approval` row with
//! `subject_kind = 'policy'` and `state = 'submitted'` — that is what
//! `find_pending_policy_unit` reads and what `PENDING_CHANGE_UNIT_EXISTS` names —
//! so the constraint that makes the rule physical belongs on the approval store, on
//! `(tenant_id)` under that predicate. The version store keeps the key it has.
//!
//! Both halves of the predicate are load-bearing:
//!
//! * **`subject_kind = 'policy'`** narrows it to the one plane where the rule is
//!   per **tenant**. Plan-revision and window units are per canonical scope key
//!   (`inst-co-single-pending`, and `pricing_approval_key` is where *that* rule is
//!   physical); one tenant holding several of those at once is the normal case, and
//!   an index without this conjunct would refuse it.
//! * **`state = 'submitted'`** is the rule itself, exactly as
//!   `uq_pricing_approval_key_pending`'s own predicate is: a decided or withdrawn
//!   unit holds nothing, which is what makes `inst-as-void`'s withdraw an escape
//!   from the pin rather than a second way to spell it. Without it the index would
//!   say "one policy proposal per tenant **ever**", and a tenant would be unable to
//!   author a second threshold version for the rest of time.
//!
//! Nothing beyond those two is wanted. `subject_ref` (the version number) is
//! deliberately *not* in the key: two proposals that lost this race disagree about
//! their currency sets and not necessarily about their number, and an index keyed on
//! the number would admit exactly the pair that produces the corrupt version.
//!
//! # It forbids no designed flow, checked rather than assumed
//!
//! `AuditSubjectKind::Policy` has exactly one writer in this gear —
//! `infra::approval::open_policy_unit`, reached from `ThresholdService::propose` and
//! `ThresholdService::retire` — and both go through the same refusal. No path opens
//! two policy units on purpose, and the decided-then-reopened flow the register's
//! own escape hatch exists for still works: a withdraw moves the holding unit to
//! `voided`, which leaves the predicate, so
//! `rest_threshold_policy::a_withdrawn_proposal_frees_the_tenant_to_propose_again`
//! is unaffected. A tombstone (D-185) is an ordinary appended version and rides the
//! same unit, so it is one proposal and not two.
//!
//! # The check stays, and the index is not a replacement for it
//!
//! D-148's arrangement, verbatim: the in-transaction read is the ordinary answer —
//! it is the only one that can **name the unit** holding the proposal open, which
//! is what an operator acts on — and the index is the invariant, which no reader
//! racing a writer can step through. Neither is the other's test.
//!
//! What the loser of the race is told is `PENDING_CHANGE_UNIT_EXISTS`, the same code
//! the check answers, because the caller's situation is identical whether they lost
//! a race or arrived second. Reaching it needed a classifier change, which
//! `approval_repo::open` carries and documents: `contention_or_db` would have
//! rendered this violation as `CONCURRENT_MUTATION` ("retry"), and a retry would
//! then be refused by the check — the right answer, one round trip late and under a
//! code that sends the caller to the wrong place first.
//!
//! # The columns, and why not more of them
//!
//! `idx_pricing_approval_subject` is `(tenant_id, state, subject_ref)` and stops
//! there. It serves the first two reads whole. The third —
//! `approval_repo::find_pending_for_plan` — adds `subject_kind` and a **prefix
//! match** on `subject_ref` (`.starts_with(format!("{plan_id}/"))`, so
//! `LIKE 'plan/%'`), and this index does **not** serve that as a range over the
//! third column, which this comment claimed until 2026-08-19.
//!
//! A plain b-tree cannot answer a `LIKE` prefix as a range under a non-`C`
//! collation: Postgres needs the column indexed with `text_pattern_ops` (or the
//! database created with `C` collation) before the planner will turn `LIKE 'x%'`
//! into `>= 'x' AND < 'y'`. What the index does buy that read is real and
//! smaller than advertised — the two leading equality columns narrow it to one
//! tenant's submitted units, and `subject_ref` is then a filter inside that, not
//! a range.
//!
//! Making the claim true is a `text_pattern_ops` opclass on the third column,
//! which changes how the *other* two reads use it and has no `SQLite`
//! counterpart at all. That is a decision to measure, not one to assert in a
//! comment, so the comment is corrected and the index is left as it stands.
//!
//! `content_hash` is deliberately **not** a fourth column: it is a blob,
//! only one of the three reads names it, and widening an index for a heap fetch
//! that one caller avoids is a cost every writer pays.
//!
//! `idx_pricing_group_membership_walk` is `(tenant_id, group_value,
//! effective_from, membership_id)` — the filter's two columns followed by the
//! walk's sort key **in the order the walk sorts**. The pair matters: the cursor
//! resumes on `(effective_from, membership_id)`, so an index stopping at
//! `effective_from` would still sort the tail of every equal-instant run.
//!
//! # About this file
//!
//! Dependency level 0 **by column**: it declares no foreign key. Its
//! `pricing_approval_key_follow_state` trigger body writes
//! `bss.pricing_approval_key`, which `000004` creates, so the chain's one binding
//! rule — a table sorts after every table it references (`migrations.rs`) — is
//! inverted here. A forward install survives it because both engines resolve
//! trigger-body names at execution rather than at creation; a rollback of `000004`
//! alone does not, and leaves this table's trigger pointing at a dropped relation
//! so that every state flip on `pricing_approval` fails.
//! Columns read identity first, then content by name, then the audit columns.
//!
//! The SQL is generated by `tasks/emit_chain.py` from the frozen schema goldens and
//! is rewritten on every run; this doc is not. What dissolved into this migration is
//! recorded in `tasks/migration-inventory.md`, which is where to look for the chain's
//! own history — nothing above narrates it, because a fresh-install chain has none.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const PG_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE bss.pricing_approval (
            tenant_id           uuid        NOT NULL,
            approval_id         uuid        NOT NULL,
            approver_principal  uuid,
            content_hash        bytea       NOT NULL,
            decided_at          timestamptz,
            materiality         jsonb       NOT NULL,
            reason              text,
            state               text        NOT NULL,
            subject_kind        text        NOT NULL,
            subject_ref         text        NOT NULL,
            submitted_at        timestamptz NOT NULL DEFAULT now(),
            submitter_principal uuid        NOT NULL,
            CONSTRAINT chk_pricing_approval_approver CHECK (state IN ('submitted','voided') OR approver_principal IS NOT NULL),
            CONSTRAINT chk_pricing_approval_decided_at CHECK ((state = 'submitted') = (decided_at IS NULL)),
            CONSTRAINT chk_pricing_approval_distinct_principals CHECK (approver_principal IS NULL OR approver_principal <> submitter_principal),
            CONSTRAINT chk_pricing_approval_reason CHECK (state <> 'rejected' OR reason IS NOT NULL),
            CONSTRAINT chk_pricing_approval_state CHECK (state IN ('submitted','approved','rejected','voided')),
            CONSTRAINT chk_pricing_approval_subject_kind CHECK (subject_kind IN ('plan_revision','price_unit','window','policy','overlay','bulk_operation','membership','taxonomy_value')),
            CONSTRAINT pricing_approval_pkey PRIMARY KEY (approval_id)
        )",
    "CREATE INDEX idx_pricing_approval_subject ON bss.pricing_approval USING btree (tenant_id, state, subject_ref)",
    "CREATE UNIQUE INDEX uq_pricing_approval_policy_pending ON bss.pricing_approval USING btree (tenant_id) WHERE ((subject_kind = 'policy'::text) AND (state = 'submitted'::text))",
    "CREATE OR REPLACE FUNCTION bss.pricing_approval_append_only() RETURNS trigger AS $$
        BEGIN
          -- Born `submitted` or not born. Tested first because it is the only
          -- branch with no OLD row to read, and because every branch below is
          -- written about a record that started pending.
          IF TG_OP = 'INSERT' THEN
            IF NEW.state <> 'submitted' THEN
              RAISE EXCEPTION
                'pricing_approval: approval % arrives %; a record is born submitted',
                NEW.approval_id, NEW.state;
            END IF;
            RETURN NEW;
          END IF;

          IF TG_OP = 'DELETE' THEN
            RAISE EXCEPTION
              'pricing_approval: DELETE of approval % is not permitted; the record is the evidence',
              OLD.approval_id;
          END IF;

          IF OLD.state <> 'submitted' THEN
            RAISE EXCEPTION
              'pricing_approval: approval % is %; a decided record is immutable',
              OLD.approval_id, OLD.state;
          END IF;

          -- The submitted plane pins everything the decision does not touch.
          -- `content_hash` is the TOCTOU guard itself; re-pinning it in place
          -- would launder the very mutation the guard exists to catch.
          IF NEW.approval_id         IS DISTINCT FROM OLD.approval_id
          OR NEW.tenant_id           IS DISTINCT FROM OLD.tenant_id
          OR NEW.subject_ref         IS DISTINCT FROM OLD.subject_ref
          OR NEW.subject_kind        IS DISTINCT FROM OLD.subject_kind
          OR NEW.content_hash        IS DISTINCT FROM OLD.content_hash
          OR NEW.submitter_principal IS DISTINCT FROM OLD.submitter_principal
          OR NEW.materiality         IS DISTINCT FROM OLD.materiality
          OR NEW.submitted_at        IS DISTINCT FROM OLD.submitted_at THEN
            RAISE EXCEPTION
              'pricing_approval: approval % is pinned; only the decision columns may move',
              OLD.approval_id;
          END IF;

          IF NEW.state NOT IN ('approved','rejected','voided') THEN
            RAISE EXCEPTION 'pricing_approval: state % -> % is not a sanctioned flip',
              OLD.state, NEW.state;
          END IF;

          RETURN NEW;
        END;
     $$ LANGUAGE plpgsql",
    "CREATE OR REPLACE FUNCTION bss.pricing_approval_key_follow_state() RETURNS trigger AS $$
        BEGIN
          IF NEW.state IS DISTINCT FROM OLD.state THEN
            UPDATE bss.pricing_approval_key
               SET state = NEW.state
             WHERE approval_id = NEW.approval_id;
          END IF;
          RETURN NEW;
        END;
     $$ LANGUAGE plpgsql",
    "CREATE TRIGGER trg_pricing_approval_append_only BEFORE INSERT OR DELETE OR UPDATE ON bss.pricing_approval FOR EACH ROW EXECUTE FUNCTION bss.pricing_approval_append_only()",
    "CREATE TRIGGER trg_pricing_approval_key_follow_state AFTER UPDATE ON bss.pricing_approval FOR EACH ROW EXECUTE FUNCTION bss.pricing_approval_key_follow_state()",
];

const PG_DOWN_STATEMENTS: &[&str] = &[
    "DROP TABLE IF EXISTS bss.pricing_approval",
    "DROP FUNCTION IF EXISTS bss.pricing_approval_append_only()",
    "DROP FUNCTION IF EXISTS bss.pricing_approval_key_follow_state()",
];

const SQLITE_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE pricing_approval (
            tenant_id           text NOT NULL,
            approval_id         text NOT NULL,
            approver_principal  text,
            content_hash        blob NOT NULL,
            decided_at          text,
            materiality         text NOT NULL,
            reason              text,
            state               text NOT NULL,
            subject_kind        text NOT NULL,
            subject_ref         text NOT NULL,
            submitted_at        text NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00'),
            submitter_principal text NOT NULL,
            PRIMARY KEY (approval_id),
            CONSTRAINT chk_pricing_approval_approver CHECK (state IN ('submitted','voided') OR approver_principal IS NOT NULL),
            CONSTRAINT chk_pricing_approval_decided_at CHECK ((state = 'submitted') = (decided_at IS NULL)),
            CONSTRAINT chk_pricing_approval_distinct_principals CHECK (approver_principal IS NULL OR approver_principal <> submitter_principal),
            CONSTRAINT chk_pricing_approval_reason CHECK (state <> 'rejected' OR reason IS NOT NULL),
            CONSTRAINT chk_pricing_approval_state CHECK (state IN ('submitted','approved','rejected','voided')),
            CONSTRAINT chk_pricing_approval_subject_kind CHECK (subject_kind IN ('plan_revision','price_unit','window','policy','overlay','bulk_operation','membership','taxonomy_value'))
        )",
    "CREATE INDEX idx_pricing_approval_subject ON pricing_approval (tenant_id, state, subject_ref)",
    "CREATE UNIQUE INDEX uq_pricing_approval_policy_pending ON pricing_approval (tenant_id) WHERE subject_kind = 'policy' AND state = 'submitted'",
    "CREATE TRIGGER trg_pricing_approval_born_submitted BEFORE INSERT ON pricing_approval FOR EACH ROW WHEN NEW.state <> 'submitted' BEGIN SELECT RAISE(ABORT, 'pricing_approval: a record is born submitted'); END",
    "CREATE TRIGGER trg_pricing_approval_flip_whitelist BEFORE UPDATE ON pricing_approval FOR EACH ROW WHEN OLD.state = 'submitted' AND NEW.state NOT IN ('approved','rejected','voided') BEGIN SELECT RAISE(ABORT, 'pricing_approval: state transition is not a sanctioned flip'); END",
    "CREATE TRIGGER trg_pricing_approval_immutable_once_decided BEFORE UPDATE ON pricing_approval FOR EACH ROW WHEN OLD.state <> 'submitted' BEGIN SELECT RAISE(ABORT, 'pricing_approval: a decided record is immutable'); END",
    "CREATE TRIGGER trg_pricing_approval_key_follow_state AFTER UPDATE OF state ON pricing_approval FOR EACH ROW WHEN NEW.state IS NOT OLD.state BEGIN UPDATE pricing_approval_key SET state = NEW.state WHERE approval_id = NEW.approval_id; END",
    "CREATE TRIGGER trg_pricing_approval_no_delete BEFORE DELETE ON pricing_approval FOR EACH ROW BEGIN SELECT RAISE(ABORT, 'pricing_approval: DELETE of an approval is not permitted; the record is the evidence'); END",
    "CREATE TRIGGER trg_pricing_approval_pinned_columns BEFORE UPDATE ON pricing_approval FOR EACH ROW WHEN OLD.state = 'submitted' AND (NEW.approval_id IS NOT OLD.approval_id OR NEW.tenant_id IS NOT OLD.tenant_id OR NEW.subject_ref IS NOT OLD.subject_ref OR NEW.subject_kind IS NOT OLD.subject_kind OR NEW.content_hash IS NOT OLD.content_hash OR NEW.submitter_principal IS NOT OLD.submitter_principal OR NEW.materiality IS NOT OLD.materiality OR NEW.submitted_at IS NOT OLD.submitted_at) BEGIN SELECT RAISE(ABORT, 'pricing_approval: the approval is pinned; only the decision columns may move'); END",
];

const SQLITE_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS pricing_approval"];

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
