//! The row-to-record reading, and the compare-and-swap — the two things the
//! public surface cannot be driven to.
//!
//! The database suite (`tests/sqlite_approval_repo.rs`) proves what the store
//! does; what is here is what that suite cannot reach.
//!
//! The first is the boundary's own judgement, which needs no database: the
//! CHECKs make the interesting rows unwritable — `chk_pricing_approval_state`
//! and `chk_pricing_approval_subject_kind` refuse them — so the only way to ask
//! "what does this repository do when the table was written around" is to hand
//! it the model directly.
//!
//! The second is [`swap`], which needs a database *and* a caller reaching the
//! `UPDATE` with a record the store has already moved past. No sequence of
//! public calls produces that: [`decide`] reads the record immediately before it
//! acts, so a decided one is turned away by §4's machine and never reaches the
//! statement. Calling the private function directly is the only way to execute
//! the guard **deterministically and single-threaded** — a concurrency test
//! would prove nothing on `SQLite`, whose single writer serializes the
//! transactions the race is made of. `idempotency_repo_tests` is the precedent
//! in this crate and `coord::lease::sqlite_tests` outside it, both for the same
//! reason.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;

use sea_orm_migration::MigratorTrait;
use serde_json::json;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

use super::{NewApproval, SUBJECT_KINDS_WITH_A_WRITER, decide, open, read, swap, to_domain};
use crate::domain::approval::{ApprovalDecision, ApprovalState};
use crate::domain::audit::{AuditStamp, AuditSubjectKind};
use crate::domain::instant::utc_ymd_hms;
use crate::infra::storage::entity::approval;
use crate::infra::storage::migrations::Migrator;
use crate::infra::storage::{RepoError, repo_failure};
use crate::source_scan::{blank_comments_and_literals, matching_brace};
use time::OffsetDateTime;

fn row(state: &str, subject_kind: &str) -> approval::Model {
    approval::Model {
        approval_id: Uuid::from_u128(0xa1),
        tenant_id: Uuid::from_u128(0x7e),
        subject_ref: "0000ffff-0000-0000-0000-000000000001/3".to_owned(),
        subject_kind: subject_kind.to_owned(),
        content_hash: vec![1, 2, 3],
        state: state.to_owned(),
        submitter_principal: Uuid::from_u128(0x5b),
        approver_principal: None,
        reason: None,
        materiality: json!({}),
        submitted_at: utc_ymd_hms(2026, 8, 3, 9, 0, 0),
        decided_at: None,
    }
}

#[test]
fn every_state_the_check_admits_reads_back_into_the_machine() {
    // The world in which the refusals below are observable. Without it the
    // corrupt-token tests would pass against a reading that refuses everything.
    for state in ApprovalState::ALL {
        let record = to_domain(row(state.as_str(), "plan_revision")).expect("a stored token reads");
        assert_eq!(record.state, *state);
    }
    for kind in AuditSubjectKind::ALL {
        let record = to_domain(row("submitted", kind.as_str())).expect("a stored token reads");
        assert_eq!(record.subject_kind, *kind);
    }
}

#[test]
fn a_state_token_outside_the_enumeration_is_a_corrupt_row_and_names_the_column() {
    // Not a caller mistake: the CHECK refuses this value, so a row holding it
    // means the table was written around. An operator needs the column named to
    // find it.
    let err = to_domain(row("withdrawn", "plan_revision"))
        .expect_err("a token no CHECK admits is not readable");
    match err {
        RepoError::CorruptRow(detail) => {
            assert!(detail.contains("pricing_approval.state"), "got: {detail}");
            assert!(detail.contains("withdrawn"), "got: {detail}");
        }
        other => panic!("expected a corrupt row, got: {other:?}"),
    }
}

#[test]
fn a_subject_kind_outside_d158s_enumeration_is_a_corrupt_row() {
    // The members S5 §6 lists and this gear does not declare. D-158 keeps the two
    // stores in step, so one of these arriving here means somebody widened one of
    // them alone.
    //
    // **The token has now moved three times, and all three moves are the same
    // event.** It was `window` until the three window surfaces mounted; then
    // `overlay`, chosen as *"the next member of S5 §6's enumeration with no writer
    // here"*; `overlay` stopped being that on 2026-08-06 (D-221's audit writer);
    // this example was then moved to `membership` — and `membership` stopped being
    // that on 2026-08-11, when `group_membership_repo` (Task 4 of the
    // customer-group plane) gave it a writer and `AuditSubjectKind::Membership` a
    // variant. Asserting any of the three unreadable now would assert the opposite
    // of what the gear does — exactly the drift this test is named for catching in
    // *stored data* and had, ironically, twice now failed to catch in *itself*.
    //
    // **The replacement is `not_a_subject_kind`, and it is synthetic rather than
    // borrowed from S5 §6 — that is the fix, not a stronger version of the same
    // pick.** Every prior probe was a real domain word and every one of them
    // eventually got declared: `window` and `overlay` because their writers
    // landed; `membership` the same way, days after it was chosen *for* being
    // undeclared. A fourth real-word pick, `historical_import`, was tried next
    // and rejected before it could repeat the pattern a third time — S5 §6
    // already promises the plane it names a `BackdateGrant` with a mandatory
    // reason and a full audit contract (`05-governance.md`), and it is a
    // declared authz resource in this crate today (`src/authz.rs`'s label,
    // roster and `ResourceType`). A plane the design set already commits to
    // auditing is exactly the kind of "next undeclared member" this test keeps
    // discovering the hard way. No domain word is safe for this probe, because
    // this test's whole claim is that an *undeclared* token is refused, and
    // every domain word here is a candidate to become declared. A token with no
    // referent in the design set or the codebase cannot be declared out from
    // under the test, because there is nothing for anyone to declare — this one
    // is spelled `not_a_subject_kind` specifically so it reads as "this is not a
    // kind" rather than as a plausible next feature to build.
    //
    // The count is deliberately not in this sentence, for the same reason the
    // previous version of this comment gave: `AuditSubjectKind::ALL` is the
    // roster, not a number restated here to go stale again.
    let err = to_domain(row("submitted", "not_a_subject_kind"))
        .expect_err("`not_a_subject_kind` is not a kind this gear declares");
    match err {
        RepoError::CorruptRow(detail) => {
            assert!(
                detail.contains("pricing_approval.subject_kind"),
                "got: {detail}"
            );
            assert!(detail.contains("not_a_subject_kind"), "got: {detail}");
        }
        other => panic!("expected a corrupt row, got: {other:?}"),
    }
}

#[test]
fn the_reading_carries_the_row_across_unchanged() {
    let record = to_domain(row("approved", "price_unit")).expect("reads");
    assert_eq!(record.approval_id, Uuid::from_u128(0xa1));
    assert_eq!(record.tenant_id, Uuid::from_u128(0x7e));
    assert_eq!(record.content_hash, vec![1, 2, 3]);
    assert_eq!(record.submitter_principal, Uuid::from_u128(0x5b));
    assert_eq!(record.subject_ref, "0000ffff-0000-0000-0000-000000000001/3");
}

/// The roster is the set of kinds this crate **actually opens a unit of**, and
/// this test is what makes that sentence checkable.
///
/// # Why the operands are not what they were
///
/// The assertion this replaces compared [`SUBJECT_KINDS_WITH_A_WRITER`] against
/// a hard-coded copy of itself, and then compared `ALL \ ROSTER` against a
/// hard-coded copy of *that*. Both operands moved with the constant: an author
/// adding a kind to the roster edited the literal in the same keystroke, and an
/// author adding a **writer** without touching the roster moved neither. So the
/// test could only ever fail on a typo, and the drift it is named for went
/// through it three times — `price_unit` (D-88), `overlay` (D-225, caught by
/// review finding Z8-5 rather than by this test) and now `membership`, whose
/// writer `ApprovalService::submit_membership_move_on` landed on 2026-08-12
/// while the roster and its own doc kept saying the plane had none.
///
/// The operand here moves **independently**: it is the crate's own source, read
/// at test time, and specifically every production construction of
/// [`NewApproval`] — the one value that can reach [`open`], and therefore the
/// one thing that makes a kind *written* on this plane. A writer cannot be added
/// without constructing one, so the scan sees the new kind the moment the writer
/// compiles, whether or not anybody remembers this file.
///
/// # Why a source scan rather than a driver
///
/// `tests/sqlite_publish_commit.rs`'s `every_declared_action_has_a_production_writer`
/// drives every audited path and reads the rows back, which is the stronger
/// shape and the one to prefer where the paths are already driven. It is not
/// available here: three of the seven kinds are opened only behind a REST route
/// with an idempotency gate, a threshold policy and an approved verdict in front
/// of them, and a census that had to build all of that would be a publish-plane
/// integration test that happens to end in an assertion about a constant.
/// `tests/rest_authz.rs`'s `every_mounted_router_is_merged_into_both_censuses`
/// is the precedent for reading this crate's sources to bind a roster to the
/// code rather than to a second copy of the roster.
///
/// # The scan itself is under test, because a probe that can be evaded silently
/// is what this replaced
///
/// The first version of the scan could be walked past by a construction that
/// merely wrapped its lines differently, and could credit a `..template()` site
/// with the *next* site's kind — both green, both invisible. So the scanner is a
/// pure function of one file's text ([`kinds_opened_in`], whose doc names the two
/// holes) and the four `the_scan_…` tests below arm it against each evasion
/// directly. Those four are the reason this test's assertion can be read as
/// evidence rather than as a coincidence about how today's seven sites are
/// formatted.
#[test]
fn the_roster_is_exactly_the_kinds_production_opens_a_unit_of() {
    let opened = kinds_production_opens();

    // The floor is the broken-parse guard `every_mounted_router_is_merged_into_both_censuses`
    // carries for the same reason: a scan that silently matched nothing would
    // make this test assert that the roster is empty, and pass the day somebody
    // emptied it.
    assert!(
        opened.len() >= 5,
        "the scan found {opened:?}, which is fewer kinds than this gear has opened units of \
         since D-88 — the scan is broken, not the roster"
    );

    let roster: BTreeSet<AuditSubjectKind> = SUBJECT_KINDS_WITH_A_WRITER.iter().copied().collect();
    assert_eq!(
        roster,
        opened,
        "the roster and the crate's own approval-unit writers disagree. Kinds a writer opens \
         and the roster omits: {:?}. Kinds the roster claims and nothing opens: {:?}. \
         `SUBJECT_KINDS_WITH_A_WRITER` and its doc comment are a maintained list — the day \
         they are wrong is the day a reader takes the roster as normative and the writer as \
         the mistake.",
        opened.difference(&roster).collect::<Vec<_>>(),
        roster.difference(&opened).collect::<Vec<_>>(),
    );

    // The roster is a *set* spelled as a slice, so the two properties a slice can
    // break and a set cannot are asserted rather than assumed: no kind twice, and
    // `AuditSubjectKind::ALL`'s order, which is the order every other roster in
    // this crate is read in.
    assert_eq!(
        SUBJECT_KINDS_WITH_A_WRITER.len(),
        roster.len(),
        "the roster names a kind twice: {SUBJECT_KINDS_WITH_A_WRITER:?}"
    );
    let in_declaration_order: Vec<AuditSubjectKind> = AuditSubjectKind::ALL
        .iter()
        .copied()
        .filter(|kind| roster.contains(kind))
        .collect();
    assert_eq!(
        SUBJECT_KINDS_WITH_A_WRITER, in_declaration_order,
        "the roster is out of `AuditSubjectKind::ALL` order"
    );
}

/// Every [`AuditSubjectKind`] some **production** source of this crate opens an
/// approval unit of.
///
/// The evidence is a `NewApproval { … subject_kind: AuditSubjectKind::X … }`
/// construction, which every writer performs and which nothing but a writer has
/// a reason to perform: `open` takes the value and the store takes it from
/// `open` alone. Every site spells the kind as a literal path — a caller passing
/// a variable would be a writer of *whatever kind it was handed*, which no
/// caller in this crate is and which this scan would (deliberately) not count,
/// since a roster cannot name a kind nobody can predict.
///
/// `*_tests.rs` files are skipped: a test opening a unit of some kind proves the
/// **store** takes it (that is `sqlite_approval_repo`'s
/// `every_subject_kind_d158_declares_is_storable_on_the_mirror`), not that
/// anything in production ever does. Counting them would make the roster mean
/// "storable", which is D-158's property and already has its own test.
///
/// # The scan is syntactic, and the two ways its first version could be evaded
/// **silently**
///
/// A probe that can be walked past without going red is worse than no probe: it
/// reports the property it no longer checks. The first version of this scan had
/// two such holes, both found by review on 2026-08-13 and neither reachable by
/// any site that exists today — which is exactly why they had to be closed on
/// purpose rather than left to be noticed by the site that first hit one.
///
/// 1. **It was line-shaped.** Detection was `ends_with("NewApproval {")`, so a
///    single-line initializer, a `#[rustfmt::skip]` site or one wrapped
///    differently was invisible — and invisible is green: the remaining six kinds
///    would still equal the roster and still clear the floor. This version reads
///    the file as *text*, so where the line breaks fall stops mattering.
/// 2. **The field window was unbounded by the initializer.** `subject_kind:` was
///    looked for in a flat 40-line window, so a site using `..template()` — which
///    names no kind of its own — silently borrowed the *next* site's literal.
///    This version bounds the search by the initializer's own matching brace, so
///    such a site now panics loudly and names its line.
///
/// Comments and string literals are blanked before any of that, so a doc comment
/// quoting a construction — this very doc quotes one — cannot be counted as a
/// writer, and a brace inside a string cannot throw the depth count off. A macro
/// that *spells* `NewApproval { subject_kind: $kind }` is still seen and refused
/// loudly for naming a non-literal; only one assembling the identifier itself
/// (`paste!`) could hide, and nothing in this crate does that.
///
/// [`kinds_opened_in`] is a pure function of one file's text for one reason: the
/// scanner's own behaviour is then testable, and the four tests below arm it
/// against each evasion instead of asserting that it happens to agree with the
/// roster today.
fn kinds_production_opens() -> BTreeSet<AuditSubjectKind> {
    let mut found = BTreeSet::new();
    for path in production_sources() {
        let text = std::fs::read_to_string(&path).expect("a readable source file");
        let kinds = kinds_opened_in(&text).unwrap_or_else(|e| panic!("{}:{e}", path.display()));
        found.extend(kinds);
    }
    found
}

/// Every kind one file's text opens a unit of, or the reason the scan cannot say.
///
/// The `Err` is a `<line>: <what>` fragment, which the caller prefixes with the
/// path. Every `Err` is a **failure of the roster's evidence**, never a tolerated
/// case: a site the scan cannot read is a site that could be writing any kind.
fn kinds_opened_in(text: &str) -> Result<Vec<AuditSubjectKind>, String> {
    const TYPE: &str = "NewApproval";
    // Positions are preserved by the blanking, so a line number computed here is
    // the line number in the file the caller read.
    let code = blank_comments_and_literals(text);
    let line_of = |at: usize| code[..at].matches('\n').count() + 1;

    let mut found = Vec::new();
    for (at, _) in code.match_indices(TYPE) {
        let before = code[..at].trim_end();
        let after = &code[at + TYPE.len()..];
        // An identifier boundary on both sides. `::` before is *not* a boundary
        // failure: `approval_repo::NewApproval { … }` is a construction, and one
        // site in this crate spells it that way.
        if before.ends_with(|c: char| c.is_alphanumeric() || c == '_')
            || after.starts_with(|c: char| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        // A brace makes it an initializer. Anything else is a type position, and
        // the keyword cases are the type's own declaration (`struct`), its impls
        // (`impl`, `impl … for`) and a signature returning it (`->`) — each of
        // which is followed by a brace that opens something other than a value.
        let Some(open) = after.find(|c: char| !c.is_whitespace()) else {
            continue;
        };
        if after.as_bytes()[open] != b'{' {
            continue;
        }
        let keyword = before
            .rsplit(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("");
        if matches!(
            keyword,
            "struct" | "impl" | "for" | "enum" | "union" | "trait"
        ) || before.ends_with("->")
        {
            continue;
        }

        let open = at + TYPE.len() + open;
        let close = matching_brace(&code, open)
            .ok_or_else(|| format!("{}: a `{TYPE}` initializer is never closed", line_of(open)))?;
        // **Bounded by this initializer's own braces**, which is hole (2) above:
        // a `..template()` site names no kind and must say so here rather than
        // read the next site's literal.
        let field = code[open..close]
            .find("subject_kind:")
            .map(|offset| open + offset + "subject_kind:".len())
            .ok_or_else(|| {
                format!(
                    "{}: constructs a `{TYPE}` and names no `subject_kind` inside the \
                     initializer; the scan cannot tell which kind it writes, and a site it \
                     cannot read is a site that could be writing any kind",
                    line_of(open)
                )
            })?;
        let value = code[field..close].trim_start();
        let rest = value.strip_prefix("AuditSubjectKind::").ok_or_else(|| {
            format!(
                "{}: opens a unit whose kind is not a literal `AuditSubjectKind::…`, but {:?}; a \
                 roster cannot name a kind chosen at run time",
                line_of(field),
                value.lines().next().unwrap_or(value)
            )
        })?;
        let variant: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let kind = AuditSubjectKind::ALL
            .iter()
            .copied()
            .find(|kind| format!("{kind:?}") == variant)
            .ok_or_else(|| {
                format!(
                    "{}: opens a `AuditSubjectKind::{variant}`, which is not a member of \
                     `AuditSubjectKind::ALL`",
                    line_of(field)
                )
            })?;
        found.push(kind);
    }
    Ok(found)
}

/// The scan sees a construction whose fields are on **one line**, which the
/// version this replaces could not.
///
/// Hole (1): detection was `ends_with("NewApproval {")`. Every site in this crate
/// wraps, so nothing was mis-read — and nothing would have been red the day one
/// did not wrap, because six kinds still equal a six-kind roster.
#[test]
fn the_scan_reads_a_single_line_initializer() {
    let source = "fn f() { let a = NewApproval { approval_id: id, subject_kind: \
                  AuditSubjectKind::Window, held_keys: keys }; }";
    assert_eq!(
        kinds_opened_in(source),
        Ok(vec![AuditSubjectKind::Window]),
        "a site is a site wherever its line breaks fall"
    );
}

/// A site that names no kind of its own is **refused**, rather than credited with
/// the next site's.
///
/// Hole (2): the field was looked for in a flat 40-line window from the opening
/// line, so a `..template()` site inside 40 lines of another one silently
/// borrowed its literal — reporting a writer of a kind that site does not write,
/// and hiding that the roster's evidence had a gap.
#[test]
fn the_scan_refuses_a_site_that_names_no_kind_of_its_own() {
    let source = "fn f() { let a = NewApproval { approval_id: id, ..template() }; \
                  let b = NewApproval { subject_kind: AuditSubjectKind::Policy }; }";
    let refusal = kinds_opened_in(source).expect_err("the borrowing site must be refused");
    assert!(
        refusal.contains("names no `subject_kind`"),
        "the refusal must name the hole it closes, got: {refusal}"
    );
}

/// A kind chosen at run time is refused loudly, which the version this replaces
/// also did — pinned here so the rewrite kept it.
#[test]
fn the_scan_refuses_a_kind_chosen_at_run_time() {
    let source = "fn f(kind: AuditSubjectKind) { let a = NewApproval { subject_kind: kind }; }";
    let refusal = kinds_opened_in(source).expect_err("a parameterised kind must be refused");
    assert!(refusal.contains("not a literal"), "got: {refusal}");
}

/// The type's own declaration, its impls, and **prose quoting a construction**
/// are all not writers.
///
/// The prose case is new and is the reason the blanking exists: this file's own
/// doc comments quote `NewApproval { … subject_kind: AuditSubjectKind::… }`, and a
/// textual scan over a production file that did the same would have counted it.
#[test]
fn the_scan_counts_neither_declarations_nor_prose() {
    // The doc comment is written the way a writer's own doc would write it —
    // opening brace at the end of the line, the field below it — because that is
    // the shape a textual scan counts as a seventh writer.
    let source = r#"
/// A writer builds one like this:
///
/// let new = NewApproval {
///     subject_kind: AuditSubjectKind::Window,
/// };
pub struct NewApproval { pub subject_kind: AuditSubjectKind }
impl NewApproval { fn new() -> Self { Self { subject_kind: AuditSubjectKind::Policy } } }
impl Default for NewApproval { fn default() -> Self { Self::new() } }
fn note() -> &'static str { "NewApproval { subject_kind: AuditSubjectKind::Overlay }" }
"#;
    assert_eq!(
        kinds_opened_in(source),
        Ok(vec![]),
        "only a value construction is a writer: a declaration, an impl, a doc comment and a \
         string are all things a textual scan would have counted"
    );
}

/// Every `.rs` file under `src`, recursively, that is not a test module.
///
/// Recursive for `rest_sources`' reason one suite over: a scan that reads one
/// directory level is a guard a reorganisation switches off silently.
fn production_sources() -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("the crate's own source tree") {
            let path = entry.expect("a readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("_tests.rs"))
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

// ---------------------------------------------------------------------------
// The compare-and-swap, against the executed `SQLite` mirror
// ---------------------------------------------------------------------------

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e_22);
const SUBMITTER: Uuid = Uuid::from_u128(0x5b_01);
const APPROVER: Uuid = Uuid::from_u128(0xab_01);
const SECOND_APPROVER: Uuid = Uuid::from_u128(0xab_02);

async fn harness() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, 0, 0)
}

fn pending(approval_id: Uuid) -> NewApproval {
    NewApproval {
        approval_id,
        tenant_id: TENANT,
        subject_ref: "0000ffff-0000-0000-0000-000000000001/3".to_owned(),
        subject_kind: AuditSubjectKind::PlanRevision,
        content_hash: vec![0xde, 0xad, 0xbe, 0xef],
        materiality: json!({ "reason": "noConfiguredThreshold" }),
        // No key: this module's subject is the compare-and-swap, and the register
        // has its own suite. `tests/sqlite_approval_repo.rs` carries the same note
        // at length.
        held_keys: std::collections::BTreeSet::new(),
    }
}

/// One value for the whole module: these tests call the repository directly,
/// where the value the HTTP edge would have established has no producer.
const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_10);

/// The stamp an audited call is made under.
fn stamp_of(actor: Uuid, when: OffsetDateTime) -> AuditStamp {
    AuditStamp {
        actor_principal_id: actor,
        recorded_at: when,
        correlation_id: TEST_CORRELATION,
    }
}

/// The wire status the caller is told, through the ladder the surface uses.
fn status(err: &RepoError) -> u16 {
    CanonicalError::from(repo_failure(err)).status_code()
}

#[tokio::test]
async fn the_second_reviewer_of_one_record_is_told_the_conflict_and_not_a_storage_fault() {
    // Two reviewers, one record, and the state both of them read. The first
    // decision lands; the second arrives at the `UPDATE` still holding the
    // pending record it read a moment earlier — which is precisely what `swap`
    // is called with here, because no sequence of public calls can produce that
    // state (`decide` re-reads immediately before it acts).
    //
    // Without `state = 'submitted'` in the predicate this statement matches on
    // the primary key and overwrites `approver_principal` and `reason` on the
    // row that *is* the evidence of who agreed. The trigger would still refuse
    // it — as a driver error, so the caller reads a 500 for a race whose whole
    // remedy is to re-read.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let id = Uuid::from_u128(0xca_01);

    open(&conn, &scope, pending(id), stamp_of(SUBMITTER, at(9)))
        .await
        .expect("open");
    decide(
        &conn,
        &scope,
        TENANT,
        id,
        ApprovalDecision::Approve,
        Some(APPROVER),
        None,
        stamp_of(APPROVER, at(11)),
    )
    .await
    .expect("the first reviewer decides");

    let err = swap(
        &conn,
        &scope,
        TENANT,
        id,
        ApprovalState::Rejected,
        Some(SECOND_APPROVER),
        Some("I disagree".to_owned()),
        at(12),
    )
    .await
    .expect_err("the record has moved past the state this caller read");

    match &err {
        RepoError::ApprovalNotPending { state, approval_id } => {
            // The state the row is *actually* in, not the one the loser read: it
            // read `submitted`, and answering with that would be a 409 saying
            // "approval X is submitted; only a submitted record is decidable".
            assert_eq!(state, "approved");
            assert_eq!(approval_id, &id.to_string());
        }
        RepoError::Db(detail) => panic!(
            "the predicate must keep the loser away from the trigger; \
             it reached it and the caller gets a 500: {detail}"
        ),
        other => panic!("expected the pendingness refusal, got: {other:?}"),
    }
    assert_eq!(
        status(&err),
        409,
        "governance sec 5 types this refusal 409, not 500"
    );

    // And the winner's decision stands, untouched.
    let stored = read(&conn, &scope, TENANT, id)
        .await
        .expect("read")
        .expect("still there");
    assert_eq!(stored.state, ApprovalState::Approved);
    assert_eq!(stored.approver_principal, Some(APPROVER));
    assert_eq!(stored.reason, None);
    assert_eq!(stored.decided_at, Some(at(11)));
}

#[tokio::test]
async fn a_swap_that_matches_no_row_is_a_refusal_and_never_a_silent_success() {
    // The other half of the compare-and-swap: the `rows_affected == 0` arm. This
    // record was never opened, so the predicate cannot be what excludes it and
    // no trigger fires — nothing but the arm stands between "the UPDATE touched
    // nothing" and a caller told its decision landed.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0xca_02);

    let err = swap(
        &conn,
        &AccessScope::for_tenant(TENANT),
        TENANT,
        id,
        ApprovalState::Approved,
        Some(APPROVER),
        None,
        at(11),
    )
    .await
    .expect_err("an UPDATE that matched nothing decided nothing");

    assert!(
        matches!(&err, RepoError::NotFound { id: named, .. } if named == &id.to_string()),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn a_foreign_scope_writes_nothing_on_the_decision_path() {
    // The BOLA case on the **write** path. `decide`'s own read refuses a foreign
    // caller first, so a suite driving only the public function proves nothing
    // about the `UPDATE`'s gate: drop `.scope_with(scope)` there and every test
    // that goes through `decide` stays green while the statement is free to
    // decide another tenant's approval.
    //
    // What is asserted is therefore the row, not the refusal: whether an
    // unscoped write is *reported* is the `rows_affected` arm's subject, and its
    // own test is above. What must hold here, whatever the caller is told, is
    // that nothing was written.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let owner = AccessScope::for_tenant(TENANT);
    let id = Uuid::from_u128(0xca_03);
    open(&conn, &owner, pending(id), stamp_of(SUBMITTER, at(9)))
        .await
        .expect("open");

    let _outcome = swap(
        &conn,
        &AccessScope::for_tenant(OTHER_TENANT),
        TENANT,
        id,
        ApprovalState::Approved,
        Some(APPROVER),
        None,
        at(11),
    )
    .await;

    let stored = read(&conn, &owner, TENANT, id)
        .await
        .expect("read")
        .expect("the record is still there");
    assert_eq!(
        stored.state,
        ApprovalState::Submitted,
        "a foreign caller decided this tenant's approval record"
    );
    assert_eq!(stored.approver_principal, None);
    assert_eq!(stored.decided_at, None);
}

#[test]
fn membership_move_subject_ref_writes_rfc3339_effective_from() {
    use crate::domain::membership_change::{MembershipMoveProposal, MembershipMoveSet};

    let at = utc_ymd_hms(2026, 1, 1, 0, 0, 0);
    let set = MembershipMoveSet::new(vec![MembershipMoveProposal {
        payer_tenant_id: Uuid::from_u128(1),
        group_value: "gold".to_owned(),
        effective_from: at,
    }])
    .expect("one payer is a valid set");
    let rendered = super::membership_move_subject_ref(&set).expect("render");
    // **The whole ref, against a literal.** This value is the approval unit's
    // key: `find_pending_for_subject` matches it whole, so a retry that renders
    // the instant one byte differently opens a second unit over the same move.
    // Asserting `contains("2026-01-01T00:00:00")` on the instant alone survived
    // every spelling that substring is a prefix of — `+00:00`, `.000Z`,
    // `.000000Z` — which is exactly the class of change that re-keys the register.
    assert_eq!(
        rendered,
        "membership-move/[{\"payer_tenant_id\":\"00000000-0000-0000-0000-000000000001\",\
         \"group_value\":\"gold\",\"effective_from\":\"2026-01-01T00:00:00.000000Z\"}]"
    );
    let json = rendered
        .strip_prefix("membership-move/")
        .expect("subject_ref prefix");
    let value: serde_json::Value = serde_json::from_str(json).expect("json");
    assert_eq!(
        value[0]["effective_from"]
            .as_str()
            .expect("effective_from must be an RFC3339 string, not a time tuple"),
        "2026-01-01T00:00:00.000000Z"
    );
}

#[test]
fn a_taxonomy_value_proposal_round_trips_through_its_ref_including_a_cleared_category() {
    use super::{
        ApprovalRecord, TAXONOMY_VALUE_REF_PREFIX, subject_taxonomy_value,
        taxonomy_value_subject_ref,
    };
    use crate::domain::overlay::ScopeValue;
    use crate::domain::taxonomy::{
        TaxCategoryPatch, TaxonomyClass, TaxonomyState, TaxonomyValuePatch, TaxonomyValueProposal,
    };

    let record_over = |subject_ref: String| ApprovalRecord {
        approval_id: Uuid::from_u128(7),
        tenant_id: Uuid::from_u128(1),
        subject_ref,
        subject_kind: AuditSubjectKind::TaxonomyValue,
        content_hash: vec![0; 32],
        state: ApprovalState::Submitted,
        submitter_principal: Uuid::from_u128(2),
        approver_principal: None,
        reason: None,
        materiality: json!({}),
        submitted_at: utc_ymd_hms(2026, 1, 1, 0, 0, 0),
        decided_at: None,
    };

    for patch in [
        TaxonomyValuePatch {
            display_name: Some("Europe".to_owned()),
            ..TaxonomyValuePatch::default()
        },
        TaxonomyValuePatch {
            state: Some(TaxonomyState::Retired),
            ..TaxonomyValuePatch::default()
        },
        TaxonomyValuePatch {
            tax_category: TaxCategoryPatch::Set("vat_standard".to_owned()),
            ..TaxonomyValuePatch::default()
        },
        // The clear must survive the wire, where a nullable member alone would
        // fold it into "keep".
        TaxonomyValuePatch {
            tax_category: TaxCategoryPatch::Clear,
            ..TaxonomyValuePatch::default()
        },
        TaxonomyValuePatch {
            tax_rate_present: Some(false),
            ..TaxonomyValuePatch::default()
        },
    ] {
        let proposal = TaxonomyValueProposal {
            class: TaxonomyClass::Region,
            value: ScopeValue::new("eu").expect("a value"),
            patch: patch.clone(),
        };
        let subject_ref = taxonomy_value_subject_ref(&proposal).expect("renders");
        assert!(
            subject_ref.starts_with(TAXONOMY_VALUE_REF_PREFIX),
            "{subject_ref}"
        );
        assert_eq!(
            subject_taxonomy_value(&record_over(subject_ref)).expect("decodes"),
            proposal,
            "{patch:?}"
        );
    }

    let corrupt = record_over("taxonomy-value/not json".to_owned());
    assert!(matches!(
        subject_taxonomy_value(&corrupt),
        Err(RepoError::CorruptRow(_))
    ));
}
