//! REST surface.
//!
//! Every route is mounted under the gear's service prefix,
//! `/bss-pricing/v1/{resource}`, with actions as sub-resource segments (D-140),
//! and every collection surface paginates with an opaque cursor (D-125).
//!
//! # The read shape, and the three families that still deviate from it
//!
//! **The pattern is the approvals pair**: a record family offers a
//! cursor-paginated `GET` on its collection *and* a `GET` on one member by id, so a
//! caller who holds an id never pages a tenant's whole set to reach one row, and a
//! caller who holds none can find what the server minted. It is recorded here
//! rather than left implicit because the alternative is what review finding Z13-9
//! found: nine families, each missing a *different* half, and no statement anywhere
//! of which half a tenth family owes.
//!
//! Complete pairs: **approvals**, **plans**, **migrations**, **bundles**,
//! **price overlays**, **price rows**. The deviations, each with the reason it
//! is one:
//!
//! * **price windows** — `GET /price-windows` lists (narrowable to one
//!   `price_id`); `/price-windows/{windowId}` is `PATCH` + `DELETE`. Same shape as
//!   price rows, mitigated the same way: the `price_id` filter is the practical
//!   by-id read.
//! * **repricing runs** and **bulk imports** — the mirror image: each has a by-id
//!   `GET` and no collection read. Deliberate rather than pending. Both are
//!   addressed by a **client-supplied** key (`run_id`, the import's client key), so
//!   a caller that opened a run already holds the id it needs, and neither is a
//!   catalog object an operator browses.
//!
//! * **customer-group memberships** — `GET /customer-groups/{group}/members`
//!   lists; `PATCH …/members/{id}` and `POST …/members/{payerId}/move` address a
//!   member by id and there is no `GET …/members/{id}`. The price-row deviation's
//!   exact shape, and it carries the same mitigation: D-125's cursor pair plus a
//!   `payer_tenant_id` filter, so the filtered page is the practical by-id read. Without
//!   the filter and the pagination the deviation has no mitigation at all, against
//!   this module's own opening sentence — and a mitigation stated only inside an
//!   `If-Match` parameter doc at `customer_groups.rs` is one no reader of this
//!   statement finds.
//!
//! Two statements a later surface should be held to: a family's list read owes
//! `limit`, `cursor` and its filters as **declared** query parameters — bound by
//! `module_test`'s `every_query_reading_route_declares_the_parameters_it_reads`,
//! which was written because a paginated route declaring none of its parameters
//! cannot be paged by a generated client at all — and a by-id read is not
//! satisfied by a mutating route at the same path, which is how three of the six
//! deviations above came to look like pairs in a route census.
//!
//! # Records this surface does not serve, and the shape the list above has no
//! entry for
//!
//! The deviation list is organised entirely around *which read half a family
//! owes*, so four findings from a CRUD census had nowhere to be
//! written down. They are here rather than nowhere, because a reader who greps this
//! statement for a table and finds it absent concludes the surface is complete.
//!
//! * **`pricing_catalog_version_ref` is written by every publish door and read by
//!   `GET /catalog-version/refs/{pendingRef}`.** That GET is the one-handle
//!   status: `pending` / `commit_observed` / `committed`, plus the version once
//!   finalize has landed. `GET /catalog-version/frontier` remains the tenant
//!   pin watermark and says nothing about one publish's outcome. The writers
//!   are still `grep -rln "catalog_version_ref_repo::record_pending" src/`.
//! * **`GET /migrated-origin-snapshots/{subscriptionRef}` reads a table no route
//!   writes.** `pricing_snapshot_provenance`'s only writers are
//!   `SynthesisService::synthesize`/`synthesize_in`, and a crate-wide grep for
//!   `.synthesize(` finds callers in **one test file** — no route, and none of the
//!   three mounted jobs (`gated_markets`, `readmodel_warm`, `window_activation`).
//!   The route's own description says *"synthesis then runs as a separate audited
//!   step"* without drawing the conclusion: that step is mounted nowhere, so on a
//!   production stand this read can only answer 404. This is the **inverse** of
//!   every entry above — a read whose record nothing produces — and the list had no
//!   shape for it.
//! * **`pricing_operator_flag` is a migrated table with no code at all**: no
//!   repository, no reader, no writer, and `operator_flag::` appears outside its
//!   entity and its migration registration nowhere. Not a REST defect; recorded
//!   here because a CRUD census is the pass that finds it and this is the document
//!   that claims to say which halves are owed.
//! * **Six of `pricing_policy_object`'s eight configurables have no route in either
//!   direction.** Two are served (`tax-display-policy`, `rounding-policy`); the six
//!   that are not are `enforced_migration_notice_days`, `max_tier_bands_per_row`,
//!   `max_price_rows_per_plan`, `max_custom_interval_days`,
//!   `max_custom_interval_months` and `additional_required_descriptors`. Four of the
//!   six are limits `config.rs`'s `LimitsConfig` also carries, so the operator-visible
//!   source of truth for a tenant limit is ambiguous between a config file and a
//!   column nothing can read.
//!
//! # Gate before parse, and the one stated exception
//!
//! **A handler asks its PDP question before it parses anything the caller sent**,
//! so a caller outside the scope learns they are denied rather than that their
//! body, header or path segment is malformed. **Two window mutations did not**,
//! and every other mutating handler already did; this paragraph exists because the
//! gear stated the rule **twice in two modules and in opposite
//! directions**, each citing itself as the standard — `bulk_imports.rs` called
//! gate-first *"this directory's stated discipline"*, `taxonomies.rs` named the
//! census property that depends on it, and `windows.rs` argued parse-first as *"the
//! shape `schedule_window` reads its idempotency key in"*. Three prose statements
//! where one was owed, and the two outliers answered 400 where both written rules
//! say 403, so the census's recorded question sequence for those routes was a claim
//! about the well-formed subset of requests only.
//!
//! **Two statements of this shape stand in the tree** — `bulk_imports.rs`'s and
//! `taxonomies.rs`' — and `schedule_window` is not a third: it gates first, because
//! reading the request first answers 400 to a caller with no authority on the window
//! plane. Quoting a withdrawn precedent in the present tense is what makes the
//! exception below read as a house pattern instead of the single argued case it is.
//!
//! Nothing was exploitable — the direction is fail-earlier — and it is stated here
//! rather than left to a fourth module doc because that is what the last three
//! attempts produced.
//!
//! **The population is deliberately not counted here**. This read *"thirty-four
//! of the thirty-seven mutating handlers already did; two window mutations did not"*,
//! whose own arithmetic is 36, and neither figure had a derivation: a handler can
//! serve two routes and
//! `grep -cE "OperationBuilder::(post|put|patch|delete)\(" src/api/rest/*.rs` counts
//! **registrations**, not handlers, so the two are not the same number and the prose
//! never said which it meant. What the exceptions are is the load-bearing half and
//! it is stated exactly; how many handlers hold the rule is a thing the reader can
//! derive and this sentence cannot keep true — the lesson `validation.rs`'s stage
//! census records after being wrong twice about its own.
//!
//! **The one exception is `GET /plans/{planId}/sellability`**, which parses its
//! three market parameters first and says why at the call site: the route is
//! `plan × read` over a *market*, and a caller who omitted `currency` has asked a
//! question that names no subject for a gate to be about. It is an exception
//! because it is argued, not because it is a read.

pub mod approvals;
pub mod audit;
pub mod auth_context;
pub mod bulk_imports;
pub mod bundles;
pub mod catalog_skus;
pub mod correlation;
pub mod cursor;
pub mod customer_groups;
pub mod cutovers;
pub mod error;
pub mod frontier;
pub mod gl_codes;
pub mod history;
pub mod migrated_origin_snapshots;
pub mod migrations;
pub(crate) mod odata_list;
pub mod overlays;
pub mod plans;
/// The operator-supplied change reason a window is recorded under, bounded.
///
/// `chk_pricing_price_window_reason_code` refuses a blank at the store, so this door
/// and the column agree about emptiness; what the column does not bound is **length**,
/// and it takes a megabyte. It is frozen by the table's append-only trigger, so
/// whatever lands is what an auditor reads forever. There is no vocabulary to check
/// against either: §5 declares none, and inventing one at a REST door is
/// the design set's decision rather than this gear's, which is why this bounds the
/// value without judging it.
///
/// # Errors
/// [`crate::domain::error::DomainError::InvalidRequest`] naming the field, for a
/// blank reason or one past [`REASON_CODE_MAX_CHARS`].
pub(crate) fn require_reason_code(
    reason_code: &str,
) -> Result<(), crate::domain::error::DomainError> {
    if reason_code.trim().is_empty() {
        return Err(crate::domain::error::DomainError::InvalidRequest(
            "reasonCode is blank; the window records it as the operator's account of the \
             change and the column it lands in is frozen once written"
                .to_owned(),
        ));
    }
    let chars = reason_code.chars().count();
    if chars > REASON_CODE_MAX_CHARS {
        return Err(crate::domain::error::DomainError::InvalidRequest(format!(
            "reasonCode is {chars} characters and the bound is {REASON_CODE_MAX_CHARS}"
        )));
    }
    Ok(())
}

/// The migration's subscription filter, bounded by size and nothing else.
///
/// **Deliberately not interpreted**: `pricing_migration.scope` is `jsonb NOT NULL`
/// carrying `all` or a filter the catalog never reads — it rides the
/// `PlanMigrationScheduled` contract to the party that does — so a shape check here
/// would be this gear inventing semantics for a document it does not own.
///
/// What is left is size. The column is frozen by the table's append-only trigger and
/// the value is copied onto an event, so an unbounded document is written once and
/// carried forever by every consumer of that contract. Depth needs no bound of its
/// own: the value arrives through `serde_json`, whose recursion limit refuses a
/// deeper document before this sees it.
///
/// # Errors
/// [`crate::domain::error::DomainError::InvalidRequest`] naming the size, past
/// [`MIGRATION_SCOPE_MAX_BYTES`].
pub(crate) fn require_bounded_scope(
    scope: &serde_json::Value,
) -> Result<(), crate::domain::error::DomainError> {
    let bytes = scope.to_string().len();
    if bytes > MIGRATION_SCOPE_MAX_BYTES {
        return Err(crate::domain::error::DomainError::InvalidRequest(format!(
            "scope renders to {bytes} bytes and the bound is {MIGRATION_SCOPE_MAX_BYTES}"
        )));
    }
    Ok(())
}

/// The status a replay answers with, read back out of the column the claim wrote.
///
/// Refused rather than defaulted, and the class is the provenance: this crate is
/// the only writer of `pricing_idempotency_dedup.response_status`, so a value
/// that is not an HTTP status is that table written around — not an author's
/// mistake. Answering `200 OK` for it would tell a caller retrying on a timeout
/// that its request succeeded, on the strength of a number nobody can read.
///
/// Spelled once because every replay door on this surface decodes the same
/// column, and a per-surface spelling is a place for the fallback to differ —
/// which it did: one door substituted `202` where its siblings substituted
/// `200`, so the surface a caller retried on decided what its answer meant.
///
/// The `operation` is carried so the alarm names the row class it was raised
/// over, as `idempotency_repo`'s own `CorruptRow` does. The client key is not:
/// most callers move it into the
/// [`GuardedRequest`](crate::infra::idempotent::GuardedRequest) and no longer
/// hold it, and threading it through the one door that does would buy a sharper
/// message only on a path a `CHECK` makes unreachable while the table is intact.
///
/// # Errors
/// [`crate::domain::error::DomainError::Internal`], through
/// [`RepoError::CorruptRow`](crate::infra::storage::RepoError::CorruptRow), so
/// the operator alarm the corruption warrants is raised.
pub(crate) fn replayed_status(
    operation: &str,
    status: i32,
) -> Result<axum::http::StatusCode, crate::domain::error::DomainError> {
    u16::try_from(status)
        .ok()
        .and_then(|code| axum::http::StatusCode::from_u16(code).ok())
        .ok_or_else(|| {
            crate::infra::storage::repo_failure(&crate::infra::storage::RepoError::CorruptRow(
                format!(
                    "the idempotency dedup row for `{operation}` carries `{status}` as its \
                     response status, which is not an HTTP status"
                ),
            ))
        })
}

#[cfg(test)]
mod replayed_status_tests {
    use super::replayed_status;

    /// The whole admitted range round-trips, including the bounds
    /// `chk_pricing_idempotency_dedup_status` sets.
    #[test]
    fn every_status_the_column_admits_decodes_to_itself() {
        for stored in 100..=599_i32 {
            let decoded = replayed_status("op", stored).expect("the column admits it");
            assert_eq!(i32::from(decoded.as_u16()), stored);
        }
        // And the band past the column's bound, which the fault case's doc rests
        // on: driven rather than described, so a narrowing of `from_u16` reddens
        // here instead of turning a decode into a 500 with both cases green.
        for stored in [600_i32, 700, 999] {
            let decoded =
                replayed_status("op", stored).expect("a status, though not one this gear issues");
            assert_eq!(i32::from(decoded.as_u16()), stored);
        }
    }

    /// Outside what a status can be, the answer is a fault and not a
    /// substituted status.
    ///
    /// Unreachable while the CHECK stands on both backends — which is the point:
    /// the branch exists for a table written around the gear, so no store-driven
    /// test can reach it and this is the only thing that can.
    ///
    /// **The refusal is wider than the column and narrower than `i32`.**
    /// `StatusCode::from_u16` admits `100..=999`, so `600` — which the column's
    /// CHECK refuses — decodes here rather than faulting. That is the right
    /// division: this helper answers "is this an HTTP status", the CHECK answers
    /// "is this a status this gear issues", and a value in between is a row
    /// written around the gear that a caller can still be told about honestly.
    #[test]
    fn a_value_the_column_could_not_hold_is_a_fault_and_names_the_operation() {
        for stored in [-1, 0, 99, 1_000, 100_000, i32::MIN, i32::MAX] {
            let err = replayed_status("create_plan", stored)
                .expect_err("a value that is not a status must not be substituted for one");
            let rendered = format!("{err:?}");
            assert!(rendered.contains("create_plan"), "{rendered}");
            assert!(rendered.contains(&stored.to_string()), "{rendered}");
        }
    }
}

/// The bound [`require_bounded_scope`] applies — room for a large explicit
/// subscription list, and short of a document nothing downstream expects.
pub(crate) const MIGRATION_SCOPE_MAX_BYTES: usize = 64 * 1024;

/// The bound [`require_reason_code`] applies.
///
/// Generous rather than tight: the field is prose an operator writes for an auditor,
/// so the number is here to stop an unbounded write into a frozen column, not to
/// shape what they say.
pub(crate) const REASON_CODE_MAX_CHARS: usize = 512;

pub mod preconditions;
pub mod preview;
pub mod prices;
pub mod publish;
pub mod repricing_runs;
pub mod retirement;
pub mod rounding_policies;
pub mod rounding_policy;
pub mod state;
pub mod supersessions;
pub mod tax_display_policy;
pub mod taxonomies;
pub mod threshold_policy;
pub mod windows;

#[cfg(test)]
mod request_bound_tests {
    use super::{
        MIGRATION_SCOPE_MAX_BYTES, REASON_CODE_MAX_CHARS, require_bounded_scope,
        require_reason_code,
    };
    use crate::domain::error::DomainError;

    /// Blank is not a reason, and whitespace is a blank wearing a value's shape.
    ///
    /// `chk_pricing_price_window_reason_code` refuses both at the store as well —
    /// this door is the half that names the field. The column is frozen by the
    /// table's append-only trigger, so whatever does land is what an auditor reads
    /// forever.
    #[test]
    fn a_blank_reason_code_is_refused_and_says_which_field() {
        for blank in ["", "   ", "\t\n"] {
            let err = require_reason_code(blank).expect_err("blank is not a reason");
            assert!(
                matches!(&err, DomainError::InvalidRequest(d) if d.contains("reasonCode")),
                "the refusal has to name the field the author edits: {err:?}"
            );
        }
    }

    /// The bound is on characters and not bytes, so a multi-byte reason is measured
    /// as an author counts it.
    #[test]
    fn a_reason_code_at_the_bound_passes_and_one_past_it_does_not() {
        let at = "\u{20ac}".repeat(REASON_CODE_MAX_CHARS);
        assert!(require_reason_code(&at).is_ok(), "the bound is inclusive");

        let past = "\u{20ac}".repeat(REASON_CODE_MAX_CHARS + 1);
        let err = require_reason_code(&past).expect_err("one past the bound is refused");
        assert!(
            matches!(&err, DomainError::InvalidRequest(d)
                if d.contains(&(REASON_CODE_MAX_CHARS + 1).to_string())),
            "and it names the size it measured, so the author knows by how much: {err:?}"
        );
    }

    /// The scope is bounded and **not** interpreted: the catalog never reads it, so
    /// an unrecognised shape inside the bound is admitted on purpose.
    #[test]
    fn an_uninterpreted_scope_passes_and_an_unbounded_one_does_not() {
        let odd = serde_json::json!({ "kind": "whatever-the-other-party-calls-it" });
        assert!(
            require_bounded_scope(&odd).is_ok(),
            "shape is the contract's business, not this door's"
        );

        let huge = serde_json::json!({ "ids": vec!["x"; MIGRATION_SCOPE_MAX_BYTES] });
        assert!(
            matches!(
                require_bounded_scope(&huge),
                Err(DomainError::InvalidRequest(_))
            ),
            "a document that rides an event contract and freezes in a column is bounded"
        );
    }
}
