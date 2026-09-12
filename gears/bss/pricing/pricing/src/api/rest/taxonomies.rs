//! `GET /bss-pricing/v1/config/taxonomies/{region|brand|partner|org_tier}` and its
//! per-value routes — the tenant's four scope-value universes (`design/04-currency-tax.md` §5, §6,
//! `inst-tx-mutation`).
//!
//! # This is the surface the gear has been missing, not a convenience
//!
//! Slice 9's `inst-plv-scope` validates every non-`global` overlay scope value
//! against one of these four tables, and Slice 4's `inst-tx-region` validates
//! every price row's region against one of them. **Both rules shipped before any
//! of the four had a writer.** The consequence was not a cosmetic gap: an
//! operator could not author a brand-scoped `PriceOverlay` end to end, because
//! the value the overlay must name had nowhere to come from, and the only way to
//! declare one was direct SQL. That is the D-211 shape — a rule whose universe is
//! unreachable — arriving from the authoring side instead of the validating one.
//!
//! # One route pair for four classes, and why the class is a path segment
//!
//! §5 writes the row as a single cell, `{region|brand|partner|org_tier}`, and the
//! four are the same resource shape over four universes: same columns, same state
//! machine, same guard, same gate. A route each would be four copies of one
//! handler differing in a `match` arm, and the day a fifth class is declared
//! (`customerGroup` is already named in Slice 9 §3, waiting on the membership
//! half) it would be a fifth copy rather than a fifth enum member.
//!
//! The class is a **path segment** and not a query parameter because it selects
//! the resource rather than filtering it: `/config/taxonomies/brand` is a
//! different document from `/config/taxonomies/partner`, with its own `ETag` and
//! its own concurrent editors. A query parameter would make them one resource
//! with four representations, and one tag would then have to cover all four.
//!
//! **The segment is the token the overlay plane stores** — `TaxonomyClass::
//! path_segment` returns `ScopeClass::as_str`, so the two cannot diverge rather
//! than merely happening not to. §5 spelled the last class `orgTier` and this
//! route answered to that, on the argument that §5 is the normative statement of
//! the route and a path segment is not a JSON field. **D-241 closed it the other
//! way**: an operator meets both spellings in one sitting, and a client generated
//! from the `OpenAPI` document carries two names for one class. `orgTier` is
//! refused rather than aliased, because two spellings that both route is the
//! state in which neither is canonical.
//!
//! # One value at a time, and the `PUT` that is gone
//!
//! `POST …/{class}/values` declares one value; `GET/PATCH …/{class}/values/{value}`
//! read and edit one (**D-353**). The whole-set `PUT` this module carried until
//! then is **removed**, not kept beside them: it could retire or re-label any
//! value without a second principal, so an approval on the per-value edit with
//! the `PUT` still standing would be a gate with an open door next to it. What
//! the `PUT` was right about survives — a value is retired, never deleted, and
//! the same two guards judge a retirement and a cleared region category — but
//! it was the wrong shape for the day-to-day edit: two admins re-labelling two
//! different regions refused each other on a set they never disagreed about, an
//! audit record of "the region list changed" could not say *which* value moved,
//! and a client saving a filtered list retired every value it did not show.
//!
//! **What is governed is a reference, not a verb.** A value nothing has
//! published against references nothing — no row is published against it yet,
//! and publishing one is already under materiality — so the `POST` commits at
//! once and needs no `Idempotency-Key`: the value is its own key, a repeat with
//! the same body replays (200) and one with other content is `409`
//! `TAXONOMY_VALUE_EXISTS` pointing at `PATCH`. **D-355 carries that same
//! argument into the edit**: while nothing published names the value, a relabel,
//! a retirement or a region's tax markers move no downstream fact either, and
//! the `PATCH` commits at once too. Once a published price row or overlay scope
//! names the value, the edit moves a universe publish validates against, and the
//! `PATCH` judges the proposal and answers `202` with the unit. The independent
//! approve applies it atomically; a second PATCH is not required. Retirement has
//! no third case — referenced is refused,
//! unreferenced is the operator's own.
//! The `PATCH` asserts the **value's own** tag ([`tag_of_value`]); the set tag
//! would reintroduce the false conflict the route exists to remove.
//!
//! # The gate is `config`, and it is one gate for every verb's subject
//!
//! `cf.bss.pricing.config.v1~` already exists and its own doc names this surface:
//! *"the tenant config plane (`write`, `read`): tax-display policy and the
//! taxonomies"*. **No authz vocabulary is minted here.** Deliberately not
//! `approval_policy`, which is segregated so a config admin cannot weaken the
//! thresholds governing their own changes — a taxonomy is the config plane's own
//! subject, and §10 assigns it to `CatalogAdmin`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::http::HeaderMap;
use axum::http::header::{ETAG, LOCATION};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::operation_builder::{ParamLocation, ParamSpec};
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::AccessScope;
use toolkit_security::SecurityContext;

use crate::api::rest::approvals::{ApprovalView, MaterialityView};
use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::preconditions;
use crate::api::rest::state::AuthoringState;
use crate::domain::approval::content_pin::taxonomy_value_content_hash;
use crate::domain::error::DomainError;
use crate::domain::materiality::{self, ChangeSet, triggers::Trigger};
use crate::domain::overlay::ScopeValue;
use crate::domain::scope_key::Region;
use crate::domain::taxonomy::{
    RegionTaxMarkers, TAXONOMY_VALUE_IN_USE, TaxCategoryPatch, TaxonomyClass, TaxonomyEntry,
    TaxonomyState, TaxonomyValueChange, TaxonomyValuePatch, TaxonomyValueProposal, ValueReferences,
    edit_is_governed, tag_of, tag_of_value,
};
use crate::infra::approval::ApprovalService;
use crate::infra::storage::repo::approval_repo;
use crate::infra::storage::repo::taxonomy_repo::{self, Declared};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;
use uuid::Uuid;

#[path = "taxonomy_pending.rs"]
mod pending;
pub use pending::{PendingContentAccess, PendingTaxonomyApprovalView, TaxonomyProposedChangesView};

/// `OpenAPI` tag applied to both operations (DE0205).
const TAG: &str = "BSS Pricing Configuration";

/// The taxonomy resource.
///
/// The literal is repeated in both `OperationBuilder` calls below because DE0801
/// validates a **literal** argument and silently passes a `const` one; the two
/// spellings are pinned together by `tests/module_test.rs`'s route census.
pub const TAXONOMY: &str = "/bss-pricing/v1/config/taxonomies/{class}";
/// One taxonomy's value collection: the per-value create.
pub const TAXONOMY_VALUES: &str = "/bss-pricing/v1/config/taxonomies/{class}/values";
/// One declared value: read and edit.
pub const TAXONOMY_VALUE: &str = "/bss-pricing/v1/config/taxonomies/{class}/values/{value}";

/// D-355: how many published things resolve through a value — the read-side twin
/// of the retire guard's counts, so a UI can show that an edit is governed before
/// the operator makes one.
///
/// Present on both list and by-value GETs, alongside `edit_governed` derived
/// from these same counts. Write responses and approval pins carry neither.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ValueReferencesView {
    /// Published price rows carrying this value on their `region` axis. Always
    /// zero outside the `region` class, which is not a price-row axis at all.
    pub published_price_rows: u64,
    /// Published overlay scopes selecting on this `(class, value)`.
    pub active_overlay_scopes: u64,
}

/// The one value a `POST …/values` declares — the authored fields and nothing
/// else.
///
/// Its own type rather than [`TaxonomyValueView`], which is what this route read
/// until D-355 added two **read-only** fields to that view. A declare body could
/// then carry `edit_governed` / `references`, `authored_entry` would ignore
/// them, and `docs/api/api.json` would advertise two settable properties the
/// server discards — the failure `authored_entry` refuses six lines into itself
/// for the tax markers, on the argument that *"a field that vanishes silently
/// reads to the operator exactly like one that failed to save"*. A field absent
/// from the request type cannot vanish silently: it is refused by the parse.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct DeclareTaxonomyValueRequest {
    /// The declared code — the string a price row's `region` or an overlay's
    /// `scopeValue` must match. Never blank.
    pub value: String,
    /// The operator's label for it.
    pub display_name: String,
    /// `active` or `retired`, defaulting to `active`.
    pub state: Option<String>,
    /// The region's default tax category (D-01). **Region taxonomy only.**
    pub tax_category: Option<String>,
    /// Tenant-declared *"a tax rate is configured for this region"* (D-01).
    /// **Region taxonomy only.**
    pub tax_rate_present: Option<bool>,
}

/// One declared value, as an operator reads it.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TaxonomyValueView {
    /// The declared code — the string a price row's `region` or an overlay's
    /// `scopeValue` must match. Never blank.
    pub value: String,
    /// The operator's label for it.
    pub display_name: String,
    /// `active` or `retired`. Optional on the way in and defaulting to `active`,
    /// because a body listing a value is a body declaring it; sending
    /// `"retired"` is the explicit spelling of the same act as leaving it out,
    /// and both are guarded identically.
    pub state: Option<String>,
    /// The region's default tax category (D-01). **Region taxonomy only** — the
    /// other three carry no such column, and a body setting it on them is
    /// refused rather than ignored.
    pub tax_category: Option<String>,
    /// Tenant-declared *"a tax rate is configured for this region"*, the MVP
    /// `RegionTaxReadiness` source (D-01). **Region taxonomy only.** Absent reads
    /// as `false`, which is C4's fail-closed default: a region nobody has
    /// declared a rate for is a region with no rate.
    pub tax_rate_present: Option<bool>,
    /// D-355: does an edit of this value need a second principal? `true` while a
    /// published price row or overlay scope names it.
    ///
    /// Filled on both `GET`s and absent everywhere else — a write response omits
    /// it, because the screen re-reads after a write.
    ///
    /// **Advisory, and outside the entity tag.** The tag covers the five
    /// authored fields above and nothing else, deliberately: a publish
    /// elsewhere in the tenant can flip this field, and a tag that moved with it
    /// would refuse every in-flight editor of a value they are not changing —
    /// the false conflict the per-value routes exist to remove. Two consequences
    /// a client must handle: a conditional read may be answered `304` while this
    /// field has since changed, so re-read unconditionally before relying on it;
    /// and the `PATCH` response (`200` or `202`) is the truth when the two
    /// disagree.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub edit_governed: Option<bool>,
    /// The counts behind [`Self::edit_governed`], on both list and by-value
    /// `GET`s, including explicit zero counts. Absent from write responses and
    /// approval pins. Outside the entity tag for [`Self::edit_governed`]'s reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub references: Option<ValueReferencesView>,
    /// Submitted proposals for this exact class/value, oldest first then by id.
    /// Both GETs always include an array (empty when none); writes and pins omit
    /// it. Metadata is config-read data; each preview additionally requires
    /// approval-read access to that unit. Outside the authored entity tag:
    /// refresh unconditionally to observe submission, withdrawal or permission changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub pending_approvals: Option<Vec<PendingTaxonomyApprovalView>>,
}

/// One taxonomy, whole.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TaxonomyView {
    /// Which universe this is — the path segment, echoed.
    pub class: String,
    /// Every declared value, `active` and `retired` alike, ordered by value.
    ///
    /// Retirements are **included**, which is what makes the round trip honest:
    /// an operator who reads, edits and writes back has to be able to see the
    /// value they are about to re-activate.
    pub values: Vec<TaxonomyValueView>,
}

/// The body of a `PATCH …/values/{value}`: only the fields to change.
///
/// Every member is optional and an absent one leaves the held value as it is.
/// `tax_category` alone distinguishes *absent* from `null`: `null` **clears** the
/// region's default category, which is a guarded act (D-245) exactly as it is on
/// the whole-set `PUT`.
#[derive(Debug, Clone, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PatchTaxonomyValueRequest {
    /// A new label.
    pub display_name: Option<String>,
    /// `active` or `retired`. A retirement is guarded (`TAXONOMY_VALUE_IN_USE`);
    /// `retired -> active` re-activates.
    pub state: Option<String>,
    /// **Region only.** A string sets the default category; `null` clears it;
    /// absent leaves it.
    #[allow(
        clippy::option_option,
        reason = "the wire distinguishes an absent member from an explicit `null`, and this is \
                  the shape that carries both; it maps to `TaxCategoryPatch` at the edge"
    )]
    #[serde(default, deserialize_with = "double_option")]
    pub tax_category: Option<Option<String>>,
    /// **Region only.**
    pub tax_rate_present: Option<bool>,
}

/// `Option<Option<T>>` the way a merge patch needs it: absent is `None`, an
/// explicit `null` is `Some(None)`, a value is `Some(Some(v))`.
///
/// `serde`'s own `Option<Option<T>>` folds `null` into the outer `None`, which
/// would make "clear the category" unsayable on the wire.
#[allow(
    clippy::option_option,
    reason = "the merge-patch reading of a nullable optional member, see the field above"
)]
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer).map(Some)
}

/// What a `PATCH …/values/{value}` answered when it opened a unit rather than
/// committing (`202`): the outcome token, the verdict the unit records and the
/// unit itself. On a commit the route answers the plain value (`200`) instead —
/// the same convergence the membership doors have, where a committed act is
/// the resource and a pending one is the unit.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TaxonomyValueMutationView {
    /// `submitted_for_approval`.
    pub outcome: String,
    /// `null` while the edit waits for a second principal.
    pub value: Option<TaxonomyValueView>,
    /// The verdict recorded on the unit — always material for this act.
    pub materiality: Option<MaterialityView>,
    /// The unit a second principal decides.
    pub approval: Option<ApprovalView>,
}

/// The `{class}` path parameter.
fn class_param() -> ParamSpec {
    ParamSpec {
        name: "class".to_owned(),
        location: ParamLocation::Path,
        required: true,
        description: Some(
            "Which value universe: `region`, `brand`, `partner` or `org_tier`. Each segment is \
             the same token the class carries in an overlay's `scopeClass` field, so a \
             generated client has one name per class (D-241); the camelCase `orgTier` section 5 \
             used to spell is refused rather than accepted as an alias. `global` and \
             `customerGroup` are deliberately not addressable: the first has no value universe, \
             and the second's table belongs to the customer-group membership half and does not \
             exist."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

/// The `{value}` path parameter.
fn value_param() -> ParamSpec {
    ParamSpec {
        name: "value".to_owned(),
        location: ParamLocation::Path,
        required: true,
        description: Some(
            "The declared code - the string a price row's `region` or an overlay's `scopeValue` \
             carries, exactly as the taxonomy lists it. Retired values are still addressable: \
             retirement is a state, not a deletion, and `PATCH` with `state: active` is the way \
             back."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        array: false,
    }
}

/// The `If-Match` header the per-value `PATCH` requires (D-171), asserting the
/// **value's own** tag rather than the set's.
fn if_match_value_param() -> ParamSpec {
    ParamSpec {
        name: "If-Match".to_owned(),
        location: ParamLocation::Header,
        required: true,
        description: Some(
            "Mandatory precondition (RFC 9110). The value is the **opaque** tag `GET \
             .../values/{value}` returns in its `ETag` header - copy it back verbatim. It \
             digests this one value's code, state, label and (region) tax markers, so it moves \
             when this value changes and **not** when a sibling does: two admins editing two \
             different values do not refuse each other, which is the reason this route exists \
             beside the whole-set `PUT`. The set's tag from `GET .../taxonomies/{class}` does \
             not satisfy it. A tag that no longer describes the value is `409` `STALE_VERSION`; \
             an absent or malformed one is `400`."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        array: false,
    }
}

/// Build the Axum router for the taxonomy operations and register them.
pub fn router(state: Arc<AuthoringState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get("/bss-pricing/v1/config/taxonomies/{class}")
        .operation_id("bss_pricing.get_taxonomy")
        .summary("Read one of the tenant's four scope-value taxonomies")
        .description(
            "Every value the tenant has declared in this universe, `active` and `retired` \
             alike, ordered by value. Retirements are included deliberately: retirement is \
             guarded rather than cascading and `retired -> active` is a legal audited move, so \
             an operator editing this list has to be able to see the value they are about to \
             re-activate. A tenant that has declared nothing is answered `200` with an empty \
             list on the brand, partner and org_tier universes - a state, not an absent \
             resource. The **region** universe is never empty: until the tenant declares a \
             region row it reads as the seeded `global` (D-354), so a fresh tenant publishes \
             `global` rows at once; `inst-tx-region` validates every price row's region \
             against the **active** values here. The response carries the set's \
             authored-content `ETag`; the per-value routes carry their own write token. On the \
             `region` universe each value also carries D-01's two markers, `taxCategory` and \
             `taxRatePresent`, which are the MVP source for the tax-display readiness check; \
             the other three universes carry no such columns. Each value also carries \
             `edit_governed` and `references` (published price-row and overlay-scope counts), \
             including explicit zeroes, matching the by-value GET without extra queries. \
             `pending_approvals` lists every submitted proposal for each value, or []. \
             Each unit exposes pending metadata and `content_access`; `proposed_changes` and \
             `content_matches_pin` appear only when the full `approval` x `read` scope \
             grants that unit. Restricted content does not hide the pending unit. \
             Tags cover authored content only, preserving write preconditions independently \
             of references and pending units. This GET always returns a fresh body with \
             `Cache-Control: private, no-store`; it does not evaluate `If-None-Match` or return \
             `304`. Gates on `config` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(class_param())
        .handler(get_taxonomy)
        .json_response_with_schema::<TaxonomyView>(
            openapi,
            StatusCode::OK,
            "The declared values of this universe.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    let router = OperationBuilder::post("/bss-pricing/v1/config/taxonomies/{class}/values")
        .operation_id("bss_pricing.declare_taxonomy_value")
        .summary("Declare one value in a scope-value taxonomy")
        .description(
            "Adds **one** value to the class without re-sending the set (D-353). `201` with the \
             value as stored, its own `ETag`, and a `Location` naming it. The value is the \
             resource's natural key, so there is no `Idempotency-Key`: a repeat carrying the \
             **same** body is the create's replay and answers `200`; a body naming a value the \
             tenant already declares with other content - or a retired one - is `409` \
             `TAXONOMY_VALUE_EXISTS`, and the remedy is `PATCH` on that value. `state` defaults \
             to `active`; `taxCategory` and `taxRatePresent` are accepted on the `region` \
             universe only and refused elsewhere. One audited config mutation naming the value. \
             Gates on `config` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(class_param())
        .json_request::<DeclareTaxonomyValueRequest>(openapi, "The one value to declare.")
        .handler(post_taxonomy_value)
        .json_response_with_schema::<TaxonomyValueView>(
            openapi,
            StatusCode::CREATED,
            "The value as declared, with its own `ETag` and `Location`.",
        )
        .json_response_with_schema::<TaxonomyValueView>(
            openapi,
            StatusCode::OK,
            "The value was already declared with exactly this content: the create's replay.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/bss-pricing/v1/config/taxonomies/{class}/values/{value}")
        .operation_id("bss_pricing.get_taxonomy_value")
        .summary("Read one declared value of a scope-value taxonomy")
        .description(
            "One value, `active` or `retired`, with **its own `ETag`** - the tag the \
                 per-value `PATCH` demands, and the only place to obtain it (the set's tag from \
                 `GET .../taxonomies/{class}` covers the whole list and does not satisfy the \
                 per-value precondition). A value the tenant has never declared is `404`. \
                 Carries `edit_governed` and `references` (D-355): whether a published price row \
                 or overlay scope names this value - and so whether editing it needs a second \
                 principal - with the count on each plane behind that answer. Also carries \
                 `pending_approvals` with the same metadata, per-unit `content_access` and \
                 approval-read-gated preview as the collection GET; current fields are not \
                 replaced by proposed ones. **These derived fields are \
                 outside the `ETag`**, which covers the declared value alone because it is also \
                 the `PATCH` precondition; moving it when an unrelated publish flips the flag \
                 would refuse every in-flight editor of a value they are not changing. \
                 This GET always returns a fresh body with `Cache-Control: private, no-store`; \
                 it does not evaluate `If-None-Match` or return `304`. The tag remains the \
                 `PATCH` precondition. Gates on `config` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(class_param())
        .param(value_param())
        .handler(get_taxonomy_value)
        .json_response_with_schema::<TaxonomyValueView>(
            openapi,
            StatusCode::OK,
            "The declared value.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router =
        OperationBuilder::patch("/bss-pricing/v1/config/taxonomies/{class}/values/{value}")
            .operation_id("bss_pricing.patch_taxonomy_value")
            .summary("Edit one declared value of a scope-value taxonomy")
            .description(
                "Changes only the fields the body names (D-353): `displayName`, `state`, and on \
                 the `region` universe `taxCategory` (a string sets it, an explicit `null` \
                 **clears** it) and `taxRatePresent`; the two markers are refused on the other \
                 three universes. **An edit is a governed act only while a published price \
                 row or overlay scope names the value** (D-355): then the first call judges the \
                 edit and, if admissible, opens an approval unit over the proposal and the value \
                 as it stands and answers `202` `submitted_for_approval` - nothing is written; \
                 the second principal's approve atomically applies the patch with the decision \
                 and audit. Re-read GET for the new value and tag; do not re-send PATCH. \
                 **While nothing published names the value, the first call commits at once and \
                 answers `200`, no unit** - as a declaration does. **Retirement is guarded** \
                 (`inst-tx-mutation`), at submit and again at commit: a value still named by a \
                 published price row or a published overlay scope is `409` \
                 `TAXONOMY_VALUE_IN_USE`; clearing a region's default category that published \
                 rows still resolve through is refused the same way (D-245). `retired -> \
                 active` re-activates. A body that changes nothing answers `200`, writes \
                 nothing and opens no unit. **`If-Match` is required** and asserts the value's \
                 **own** tag from `GET .../values/{value}`. The commit is one audited config \
                 mutation naming the value, its state before and after, and the unit it ran \
                 under. Gates on `config` x `write`.",
            )
            .tag(TAG)
            .authenticated()
            .no_license_required()
            .param(class_param())
            .param(value_param())
            .param(if_match_value_param())
            .json_request::<PatchTaxonomyValueRequest>(openapi, "The fields to change.")
            .handler(patch_taxonomy_value)
            .json_response_with_schema::<TaxonomyValueView>(
                openapi,
                StatusCode::OK,
                "An ungoverned edit committed, a legacy approved unit was applied, or nothing \
                 changed: the value as it now stands, with its `ETag`.",
            )
            .json_response_with_schema::<TaxonomyValueMutationView>(
                openapi,
                StatusCode::ACCEPTED,
                "The edit is material and waits for a second principal: `outcome` is \
                 `submitted_for_approval` and `approval` names the unit. Nothing was written.",
            )
            .error_400(openapi)
            .error_401(openapi)
            .error_403(openapi)
            .error_404(openapi)
            .error_409(openapi)
            .error_500(openapi)
            .error_503(openapi)
            .register(router, openapi);

    // D-178's edge, at this router's own tail for the reason every other mutating
    // router applies it at its own: a surface reachable without it cannot build
    // an `AuditStamp`.
    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

/// `GET /config/taxonomies/{class}`.
///
/// Answers a [`Response`] rather than a [`Json`] because it carries the set's
/// Authored-content tag only; derived data is always read afresh.
async fn get_taxonomy(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(class): Path<String>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = read_scope(&enforcer, &ctx).await?;
    // **After the gate, deliberately.** A caller who may not read this resource
    // is told that, rather than being told their path segment is unknown — the
    // ordering `rest_authz`'s `every_route_asks_the_catalogued_pair` depends on.
    let class = parse_class(&class)?;

    let held = state
        .taxonomies
        .list(&scope, ctx.subject_tenant_id(), class)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    // D-355: which of these an operator may edit alone, resolved once for the
    // whole list so the screen can label its buttons before the first edit.
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: taxonomy reference read: {e}")).create()
    })?;
    let referenced =
        taxonomy_repo::references_for_values(&conn, ctx.subject_tenant_id(), class, &held)
            .await
            .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    let pending = pending::for_values(&state, &enforcer, &ctx, &scope, class, &held).await?;
    Ok(preconditions::fresh_read(render(
        class,
        &held,
        Some(&pending),
        Some(&referenced),
    )))
}

/// `GET /config/taxonomies/{class}/values/{value}`.
async fn get_taxonomy_value(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path((class, value)): Path<(String, String)>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = read_scope(&enforcer, &ctx).await?;
    // After the gate, for `get_taxonomy`'s reason.
    let class = parse_class(&class)?;
    let value = parse_value(&value)?;

    let held = state
        .taxonomies
        .find_value(&scope, ctx.subject_tenant_id(), class, &value)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
        .ok_or_else(|| value_not_found(class, &value))?;
    // D-355: the counts as well as the flag, because this is the read a drawer
    // showing *why* an edit is governed is opened from.
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: value reference read: {e}")).create()
    })?;
    let refs = taxonomy_repo::references_to(&conn, ctx.subject_tenant_id(), class, &value)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let mut pending = pending::for_values(
        &state,
        &enforcer,
        &ctx,
        &scope,
        class,
        std::slice::from_ref(&held),
    )
    .await?;
    Ok(preconditions::fresh_read(render_value(
        class,
        &held,
        StatusCode::OK,
        Some(pending.remove(held.value.as_str()).unwrap_or_default()),
        Some(refs),
    )))
}

/// `POST /config/taxonomies/{class}/values`.
async fn post_taxonomy_value(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(class): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = write_scope(&enforcer, &ctx).await?;
    let tenant = ctx.subject_tenant_id();
    let now = OffsetDateTime::now_utc();

    let class = parse_class(&class)?;
    let request: DeclareTaxonomyValueRequest = preconditions::parse_body(&body)?;
    let entry = authored_entry(class, request)?;

    let declared = state
        .taxonomies
        .declare_value(
            &scope,
            tenant,
            class,
            entry,
            audit_stamp(&ctx, now, correlation),
        )
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    match declared {
        Declared::Created(entry) => {
            Ok(render_value(class, &entry, StatusCode::CREATED, None, None))
        }
        Declared::Replayed(entry) => Ok(render_value(class, &entry, StatusCode::OK, None, None)),
        Declared::Exists(existing) => Err(CanonicalError::from(DomainError::TaxonomyValueExists(
            format!(
                "`{}` is already declared in the {class} taxonomy ({}); a value is its own key, \
                 so declare it once and edit it with PATCH {}",
                existing.value,
                existing.state,
                TAXONOMY_VALUE
                    .replace("{class}", class.path_segment())
                    .replace("{value}", existing.value.as_str())
            ),
        ))),
    }
}

/// `PATCH /config/taxonomies/{class}/values/{value}` — the governed door (D-353).
///
/// `customer_groups::move_membership_set`'s two-arm shape, on a subject with no
/// draft table. Governed only while a published price row or overlay scope names
/// the value (D-355); otherwise the first call commits at once, `200`, no unit.
/// **First arrival of a governed edit**: the edit is judged now — an unknown value is
/// `404`, a moved tag `409 STALE_VERSION`, a guarded retirement or cleared
/// category `409 TAXONOMY_VALUE_IN_USE` — so a unit that could never commit is
/// never opened; then the always-material unit is opened over the proposal and
/// the value as held, and the call answers `202`. **Nothing is written to the
/// taxonomy on this call.** The independent approve applies the patch in its
/// own decision transaction. The existing approved-content arm below remains
/// available for legacy units approved before atomic application was introduced;
/// it does not mass-replay old units. New clients re-read GET after approval.
/// A body that changes
/// nothing is `200` with the value as it stands and opens no unit.
async fn patch_taxonomy_value(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path((class, value)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = write_scope(&enforcer, &ctx).await?;
    let tenant = ctx.subject_tenant_id();

    let class = parse_class(&class)?;
    let value = parse_value(&value)?;
    let asserted = preconditions::if_match_policy(&headers).map_err(CanonicalError::from)?;
    let request: PatchTaxonomyValueRequest = preconditions::parse_body(&body)?;
    let patch = authored_patch(class, &value, request)?;

    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: taxonomy value lookup: {e}")).create()
    })?;
    let held = taxonomy_repo::find_value_on(&conn, &scope, tenant, class, &value)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
        .ok_or_else(|| value_not_found(class, &value))?;
    if tag_of_value(class, &held) != asserted {
        return Err(CanonicalError::from(DomainError::StaleVersion(format!(
            "the If-Match tag no longer describes `{value}` in the {class} taxonomy: it changed \
             after you read it. Re-read GET {} and author against the tag it hands back",
            TAXONOMY_VALUE
                .replace("{class}", class.path_segment())
                .replace("{value}", value.as_str())
        ))));
    }
    let change = TaxonomyValueChange {
        proposal: TaxonomyValueProposal {
            class,
            value: value.clone(),
            patch,
        },
        held,
    };
    let next = change.next();
    if next == change.held {
        // Not an act: no unit, no record — the value as it stands, under its tag.
        return Ok(render_value(
            class,
            &change.held,
            StatusCode::OK,
            None,
            None,
        ));
    }
    // Judged on this arm too (D-350's lesson): a retirement the guard refuses
    // must not open a unit that can never commit.
    let report = taxonomy_repo::judge_value_patch(&conn, tenant, class, &change.held, &next)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    if let Some(violation) = report.violations.first() {
        debug_assert_eq!(violation.code, TAXONOMY_VALUE_IN_USE);
        return Err(CanonicalError::from(DomainError::TaxonomyValueInUse(
            violation.detail.clone(),
        )));
    }

    // D-355: while nothing published names the value, an edit is the operator's
    // own — commit at once, no unit, exactly as `POST` declare does. Only a
    // value a published price row or overlay scope references reaches the
    // governed door below.
    let references = taxonomy_repo::references_to(&conn, tenant, class, &value)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    if !edit_is_governed(references) {
        // **A unit already pending over this exact edit still refuses.**
        //
        // The governed arm reaches this refusal inside
        // `ApprovalService::submit_taxonomy_value_on`; this arm returns before
        // it, so without the check here a reference that legitimately goes away
        // — an overlay superseded or retired, a price row superseded — would let
        // the same edit be re-sent and commit unilaterally, leaving its unit
        // `submitted` over a change that already happened. That unit is not
        // decidable afterwards (the approve fails closed on the re-derived pin,
        // D-198) but it never leaves the reviewer's queue, and the `409` the
        // route documents would have stopped applying on one arm with no call
        // site changing.
        //
        // Scoped to *this* edit and not to the value, because the subject ref
        // carries the whole proposal: a different edit of the same value has a
        // different subject and is not what this refusal is about, here or on
        // the governed arm.
        let pending_subject = approval_repo::taxonomy_value_subject_ref(&change.proposal)
            .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
        if let Some(pending) =
            approval_repo::find_pending_for_subject(&conn, tenant, &pending_subject)
                .await
                .map_err(|e| CanonicalError::from(repo_failure(&e)))?
        {
            return Err(CanonicalError::from(DomainError::PendingChangeUnitExists(
                format!(
                    "approval {} is already pending over this exact edit of `{}` in the {class} \
                    taxonomy; decide or reject it rather than re-sending",
                    pending.approval_id, value
                ),
            )));
        }

        let stamp = audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
        let commit_scope = scope.clone();
        let committed_change = change.clone();
        let (_, outcome) = state
            .db
            .db()
            .in_transaction::<TaxonomyEntry, DomainError, _>(move |txn| {
                Box::pin(async move {
                    ApprovalService::commit_taxonomy_value_direct_in(
                        txn,
                        &commit_scope,
                        tenant,
                        &committed_change,
                        stamp,
                    )
                    .await
                })
            })
            .await;
        let committed = outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!(
                    "bss-pricing: taxonomy value direct commit: {infra}"
                ))
            })
        })?;
        return Ok(render_value(class, &committed, StatusCode::OK, None, None));
    }

    let subject_ref = approval_repo::taxonomy_value_subject_ref(&change.proposal)
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let pin = taxonomy_value_content_hash(&change);
    let approved =
        approval_repo::find_approved_for_content(&conn, &scope, tenant, &subject_ref, &pin)
            .await
            .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    if let Some(approved) = approved {
        // Compatibility for previously approved but unapplied units only. New
        // approvals already applied the patch and moved the original value tag.
        let stamp = audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
        let commit_scope = scope.clone();
        let (_, outcome) = state
            .db
            .db()
            .in_transaction::<TaxonomyEntry, DomainError, _>(move |txn| {
                Box::pin(async move {
                    ApprovalService::commit_taxonomy_value_in(
                        txn,
                        &commit_scope,
                        tenant,
                        &approved,
                        stamp,
                    )
                    .await
                })
            })
            .await;
        let committed = outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!("bss-pricing: taxonomy value commit: {infra}"))
            })
        })?;
        return Ok(render_value(class, &committed, StatusCode::OK, None, None));
    }

    let verdict = materiality::evaluate(
        &ChangeSet::of_act(Trigger::TaxonomyValueMutation, Vec::new()),
        /* policy */ None,
        /* baseline */ None,
    );
    let (_reason, stored_materiality) = crate::api::rest::overlays::rendered_materiality(&verdict)?;
    let stamp = audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
    let submit_scope = scope.clone();
    let (_, outcome) = state
        .db
        .db()
        .in_transaction::<approval_repo::ApprovalRecord, DomainError, _>(move |txn| {
            Box::pin(async move {
                ApprovalService::submit_taxonomy_value_on(
                    txn,
                    &submit_scope,
                    tenant,
                    &change,
                    Uuid::now_v7(),
                    stored_materiality,
                    stamp,
                )
                .await
            })
        })
        .await;
    let opened = outcome.map_err(|err| {
        err.into_domain(|infra| {
            DomainError::Internal(format!("bss-pricing: taxonomy value submit: {infra}"))
        })
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(TaxonomyValueMutationView {
            outcome: crate::api::rest::publish::OUTCOME_SUBMITTED.to_owned(),
            value: None,
            materiality: Some(MaterialityView::from(&verdict)),
            approval: Some(ApprovalView::from(&opened)),
        }),
    )
        .into_response())
}

/// The 404 for a value the tenant never declared. `NotFound`'s own shape: no
/// existence leaks past the scope, and the subject noun names the resource.
fn value_not_found(class: TaxonomyClass, value: &ScopeValue) -> CanonicalError {
    CanonicalError::from(DomainError::NotFound {
        subject: format!("{class} taxonomy value"),
        id: value.as_str().to_owned(),
    })
}

/// Resolve the `{value}` segment, refusing what `authored_entry` refuses.
fn parse_value(segment: &str) -> Result<ScopeValue, CanonicalError> {
    ScopeValue::new(segment).ok_or_else(|| {
        CanonicalError::from(DomainError::InvalidRequest(
            "a taxonomy value must not be blank or whitespace".to_owned(),
        ))
    })
}

/// Turn a `PATCH` body into the patch, refusing what the class cannot hold.
///
/// The tax markers are refused on the three non-region classes rather than
/// ignored, for [`authored_entry`]'s reason.
fn authored_patch(
    class: TaxonomyClass,
    value: &ScopeValue,
    request: PatchTaxonomyValueRequest,
) -> Result<TaxonomyValuePatch, CanonicalError> {
    let state = match request.state.as_deref() {
        None => None,
        Some(token) => Some(TaxonomyState::parse(token).ok_or_else(|| {
            CanonicalError::from(DomainError::InvalidRequest(format!(
                "value `{value}` carries state `{token}`; a taxonomy value is `active` or \
                 `retired`, and nothing else"
            )))
        })?),
    };
    let patch = TaxonomyValuePatch {
        display_name: request.display_name,
        state,
        tax_category: match request.tax_category {
            None => TaxCategoryPatch::Keep,
            Some(None) => TaxCategoryPatch::Clear,
            Some(Some(category)) => TaxCategoryPatch::Set(category),
        },
        tax_rate_present: request.tax_rate_present,
    };
    if !class.carries_tax_markers() && patch.touches_tax_markers() {
        return Err(CanonicalError::from(DomainError::InvalidRequest(format!(
            "value `{value}`: taxCategory and taxRatePresent are declared on the region taxonomy \
             alone (D-01) and the {class} taxonomy has no column for either. They are refused \
             rather than dropped, because a field that vanishes silently reads to the operator \
             exactly like one that failed to save"
        ))));
    }
    Ok(patch)
}

/// One value's representation, with **its own** tag (and, on a create, where it
/// now lives).
///
/// One renderer for the three per-value verbs, for [`render`]'s reason: the tag a
/// `GET` hands out and the tag a `PATCH` answers with must come from one
/// computation over one reading. `conditional` is `Some` on the `GET` alone.
/// One value's entity tag. **The only producer** — a caller that must decide a
/// conditional read *before* doing further work calls this and then renders with
/// `conditional: None`, so the comparison still happens exactly once per request
/// and against the same string the response carries.
fn value_tag(class: TaxonomyClass, entry: &TaxonomyEntry) -> String {
    preconditions::policy_etag(&tag_of_value(class, entry))
}

/// The set's entity tag — [`value_tag`]'s counterpart, same contract.
fn set_tag(class: TaxonomyClass, entries: &[TaxonomyEntry]) -> String {
    preconditions::policy_etag(&tag_of(class, entries))
}

fn render_value(
    class: TaxonomyClass,
    entry: &TaxonomyEntry,
    status: StatusCode,
    pending: Option<Vec<PendingTaxonomyApprovalView>>,
    refs: Option<ValueReferences>,
) -> Response {
    let tag = value_tag(class, entry);
    // D-355's read-side signal, and `None` at every write call site rather than a
    // second renderer beside this one: authored tags have one producer, and
    // conditional reads are decided in GET before any enrichment work.
    let mut view = view_of(entry);
    view.pending_approvals = pending;
    if let Some(refs) = refs {
        view.edit_governed = Some(edit_is_governed(refs));
        view.references = Some(ValueReferencesView {
            published_price_rows: refs.published_price_rows,
            active_overlay_scopes: refs.active_overlay_scopes,
        });
    }
    let body = Json(view);
    if status == StatusCode::CREATED {
        let location = TAXONOMY_VALUE
            .replace("{class}", class.path_segment())
            .replace("{value}", entry.value.as_str());
        return (status, [(ETAG, tag), (LOCATION, location)], body).into_response();
    }
    (status, [(ETAG, tag)], body).into_response()
}

/// The set's representation, with the tag that covers it.
///
/// The GET checks its authored tag before reading any derived enrichment.
fn render(
    class: TaxonomyClass,
    entries: &[TaxonomyEntry],
    pending: Option<&BTreeMap<String, Vec<PendingTaxonomyApprovalView>>>,
    referenced: Option<&BTreeMap<String, ValueReferences>>,
) -> Response {
    let tag = set_tag(class, entries);
    // Reuse the counts already read for the flag; exposing them adds no query.
    let values = entries
        .iter()
        .map(|entry| {
            let mut view = view_of(entry);
            view.pending_approvals =
                pending.map(|units| units.get(entry.value.as_str()).cloned().unwrap_or_default());
            if let Some(refs) = referenced.and_then(|refs| refs.get(entry.value.as_str())) {
                view.edit_governed = Some(edit_is_governed(*refs));
                view.references = Some(ValueReferencesView {
                    published_price_rows: refs.published_price_rows,
                    active_overlay_scopes: refs.active_overlay_scopes,
                });
            }
            view
        })
        .collect();
    (
        [(ETAG, tag)],
        Json(TaxonomyView {
            class: class.path_segment().to_owned(),
            values,
        }),
    )
        .into_response()
}

pub(crate) fn view_of(entry: &TaxonomyEntry) -> TaxonomyValueView {
    TaxonomyValueView {
        value: entry.value.as_str().to_owned(),
        display_name: entry.display_name.clone(),
        state: Some(entry.state.as_str().to_owned()),
        tax_category: entry.tax.as_ref().and_then(|t| t.tax_category.clone()),
        tax_rate_present: entry.tax.as_ref().map(|t| t.tax_rate_present),
        // The D-355 signal belongs to a read, and this renders writes and the
        // approval pin's before/after too — filled only by the two GET helpers.
        edit_governed: None,
        references: None,
        pending_approvals: None,
    }
}

/// Resolve the path segment, refusing the two classes that are not addressable.
///
/// The refusal names them rather than answering a bare 404, because `global` and
/// `customerGroup` are real scope classes an operator has met in the overlay
/// surface — being told the segment is unknown would send them looking for a typo
/// in a word they spelled correctly.
fn parse_class(segment: &str) -> Result<TaxonomyClass, CanonicalError> {
    TaxonomyClass::parse_segment(segment).ok_or_else(|| {
        CanonicalError::from(DomainError::InvalidRequest(format!(
            "unknown taxonomy `{segment}`: the addressable universes are region, brand, partner \
             and org_tier — each spelled as the class's own scope token (D-241; the camelCase \
             `orgTier` this route used to answer to is refused, not aliased). `global` has no \
             value universe — the classless scope carries no value — and `customerGroup`'s \
             taxonomy belongs to the customer-group membership plane and does not exist yet"
        )))
    })
}

/// One authored value as a domain entry — the `POST`'s body.
fn authored_entry(
    class: TaxonomyClass,
    value: DeclareTaxonomyValueRequest,
) -> Result<TaxonomyEntry, CanonicalError> {
    let declared = ScopeValue::new(&value.value).ok_or_else(|| {
        CanonicalError::from(DomainError::InvalidRequest(
            "a taxonomy value must not be blank or whitespace: the empty string is the store's \
             sentinel for the classless overlay scope, so a blank value here would make that \
             sentinel forgeable"
                .to_owned(),
        ))
    })?;
    // **A control character is refused here rather than at the `Location` header.**
    //
    // `ScopeValue::new` trims and refuses a blank, and nothing else — so a value
    // carrying `\n` or `\u{0}` is written to the taxonomy and then interpolated
    // into this route's `Location`, where the header conversion fails and a create
    // that already committed is answered `500`. The row is real and the operator is
    // told the request failed. Refused at the door, and only control characters
    // are: a taxonomy value is operator-authored text, so anything printable —
    // including non-Latin scripts — stays authorable.
    if let Some(bad) = declared.as_str().chars().find(|c| c.is_control()) {
        return Err(CanonicalError::from(DomainError::InvalidRequest(format!(
            "a taxonomy value must not carry a control character (found U+{:04X}): the value is \
             interpolated into this route's `Location` header and into scope-key renderings, \
             neither of which can hold one",
            u32::from(bad)
        ))));
    }
    // **The region vocabulary is narrower than `ScopeValue`, and this door is
    // what has to enforce the difference.**
    //
    // `ScopeValue::new` refuses a blank and nothing else; `Region::new` also
    // refuses `KEY_SEPARATOR`, because a region carrying `|` renders the same
    // canonical scope key as a different key does. Every *reader* of a declared
    // region rebuilds it through `Region::new` — `taxonomy_repo::active_regions`,
    // `publish::rule_params`, `prices::require_declared_region` — so a value
    // admitted here but refused there is not a value that misbehaves later: it is
    // a `RepoError::CorruptRow` for the whole tenant's region universe, raised on
    // reads the declaring operator never made, and reported as stored corruption
    // rather than as the bad request it was. Refused at the door, the class of
    // failure disappears instead of moving.
    if class == TaxonomyClass::Region {
        Region::new(declared.as_str()).map_err(CanonicalError::from)?;
    }
    let state = match value.state.as_deref() {
        None => TaxonomyState::Active,
        Some(token) => TaxonomyState::parse(token).ok_or_else(|| {
            CanonicalError::from(DomainError::InvalidRequest(format!(
                "value `{declared}` carries state `{token}`; a taxonomy value is `active` or \
                 `retired`, and nothing else — retirement is guarded and re-activation is a \
                 legal audited move, so a third state would be one no rule describes"
            )))
        })?,
    };
    if !class.carries_tax_markers()
        && (value.tax_category.is_some() || value.tax_rate_present.is_some())
    {
        return Err(CanonicalError::from(DomainError::InvalidRequest(format!(
            "value `{declared}`: taxCategory and taxRatePresent are declared on the region \
             taxonomy alone (D-01) and the {class} taxonomy has no column for either. They \
             are refused rather than dropped, because a field that vanishes silently reads \
             to the operator exactly like one that failed to save"
        ))));
    }
    Ok(TaxonomyEntry {
        value: declared,
        display_name: value.display_name,
        state,
        tax: class.carries_tax_markers().then(|| RegionTaxMarkers {
            tax_category: value.tax_category,
            tax_rate_present: value.tax_rate_present.unwrap_or(false),
        }),
    })
}

/// The `config × read` gate.
async fn read_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::CONFIG,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// The `config × write` gate.
///
/// `owner_tenant_id = Some(caller's tenant)` because this is a write, for
/// `threshold_policy::write_scope`'s reason: the membership assertion is what
/// refuses a target outside the compiled scope, the degraded flat-`In` decision
/// not re-checking the property.
async fn write_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::CONFIG,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(ctx.subject_tenant_id())),
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}
