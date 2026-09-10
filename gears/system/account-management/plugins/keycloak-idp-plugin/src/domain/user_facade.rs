//! `UserFacade` — provision/deprovision/list users against Keycloak per DESIGN "Interactions & Sequences".
//!
//! Implements `provision_user_inner`, `deprovision_user_inner`, and
//! `list_users_inner` across all three [`RealmBinding`] modes (Shared /
//! Adopted / Created). Metadata decode, secret-source selection (DESIGN "Domain Model"),
//! and user-operation audit events are handled within this facade; the SDK
//! boundary in `idp_impl.rs` translates [`PluginError`] into the typed
//! `IdpUserOperationFailure` variants (`Rejected` / `Unavailable` /
//! `UnsupportedOperation`).
//!
//! [`TenantFacade`]: crate::domain::tenant_facade::TenantFacade

use std::sync::Arc;
use std::time::Instant;

use account_management_sdk::idp_user::{
    IdpDeprovisionUserRequest, IdpListUsersRequest, IdpProvisionUserRequest, IdpUser,
    IdpUserFilterField,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use credstore_sdk::SecretRef;
use serde::Deserialize;
use toolkit_odata::filter::{FilterNode, FilterOp, ODataValue};
use toolkit_odata::{CursorV1, Page, PageInfo, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{RealmAdminConfig, UserFacadeConfig};
use crate::domain::audit;
use crate::domain::error::{KcStatusKind, PluginError, redact_secrets};
use crate::domain::kc::KcAdminClient;
use crate::domain::kc::factory::{KeycloakAdminClientFactory, SecretSource};
use crate::domain::kc::transport::truncate_2kb;
use crate::domain::kc_provisioning::encode_component;
use crate::domain::metadata_codec::{MetadataCodec, RealmBinding, TenantIdpMetadataV1};
use crate::domain::ports::metrics::{
    FailureMetricsPort, FailureVariant, MetadataCodecMetricsPort, OrphanCompensationOutcome,
    PluginOp, UserOp, UserOpMetricsPort, VersionObserved,
};
use toolkit_macros::domain_model;

// `failure_variant_label` lives in `crate::domain::error` (single source of truth).

/// Re-tag any [`PluginError::KcRest`] surfaced inside the user-op saga as a
/// [`PluginError::UserOpUnavailable`] per DESIGN "API Contracts" — transport / timeout /
/// 5xx upstream failures all map to the "provider is unreachable" lane Phase
/// 15 boundary translates to [`IdpUserOperationFailure::Unavailable`].
/// Non-[`PluginError::KcRest`] variants (e.g.
/// [`PluginError::CredStoreRead`], [`PluginError::BootstrapPermsMissing`])
/// pass through unchanged so the Phase 15 boundary can translate them.
///
/// [`IdpUserOperationFailure::Unavailable`]: account_management_sdk::idp_user::IdpUserOperationFailure::Unavailable
fn user_translate_kc(e: PluginError) -> PluginError {
    match e {
        // Preserve `KcRest{Http(401)}` verbatim so any enclosing
        // `with_reactive_401` wrapper can detect the rotation signal and
        // retry with a refreshed bearer. Without this carve-out the
        // wrapper would never see a 401 (everything would already be
        // collapsed into `UserOpUnavailable`). A 401 that escapes the
        // wrapper (i.e. the second-strike scenario) falls through the
        // Phase-15 boundary to `IdpUserOperationFailure::Unavailable`
        // via the catch-all in `translate_user_op_failure` (idp_impl.rs).
        e @ PluginError::KcRest {
            status: KcStatusKind::Http(401),
            ..
        } => e,
        PluginError::KcRest {
            method,
            path_template,
            status,
            body_first_2kb,
        } => PluginError::UserOpUnavailable {
            detail: format!("kc:{method} {path_template} -> {status}: {body_first_2kb}"),
        },
        other => other,
    }
}

/// Attribute a KC 409 user-create body to the colliding unique field
///. The HTTP 409 status itself is the machine signal that a
/// uniqueness invariant fired — on this endpoint it has no other cause
/// — so the caller classifies EVERY 409 as a duplicate and this
/// function only refines which field, never whether.
///
/// KC's admin REST emits machine constants in `errorMessage` (from KC
/// code, not the locale-dependent login theme):
/// `"User exists with same username"`, `"User exists with same
/// email"`, and the combined `"User exists with same username or
/// email"` from the `ModelDuplicateException` path — on KC 26 the
/// combined constant is the ONLY 409 `createUser` produces directly.
/// Matched case-insensitively on the discriminating nouns; a message
/// naming both fields, an unparseable body, or unrecognised wording
/// all yield the honest [`UsernameOrEmail`] attribution — wording
/// drift can degrade the field token, never the 409 status.
///
/// [`UsernameOrEmail`]: account_management_sdk::IdpUserDuplicateField::UsernameOrEmail
fn classify_kc_conflict(body_text: &str) -> account_management_sdk::IdpUserDuplicateField {
    use account_management_sdk::IdpUserDuplicateField as F;
    let msg = serde_json::from_str::<serde_json::Value>(body_text)
        .ok()
        .and_then(|v| v.get("errorMessage")?.as_str().map(str::to_ascii_lowercase));
    let Some(msg) = msg else {
        return F::UsernameOrEmail;
    };
    match (msg.contains("username"), msg.contains("email")) {
        (true, false) => F::Username,
        (false, true) => F::Email,
        _ => F::UsernameOrEmail,
    }
}

/// Detect a KC 400 password-policy reject on user-create.
/// KC signals policy failures in two shapes depending on version/path:
/// `{"errorMessage": "Password policy not met"}` (admin user-create with
/// an embedded credential) and `{"error": "invalidPasswordMinLengthMessage",
/// "error_description": ...}` (credential endpoints). Both carry an
/// `invalidPassword*` key or the literal "password policy" phrase; other
/// 400s (malformed payload, unknown fields) return `false` and keep the
/// current handling.
fn is_kc_password_policy_reject(body_text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body_text) else {
        return false;
    };
    let field_str = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    let error_message = field_str("errorMessage");
    let error = field_str("error");
    error_message.contains("password policy")
        || error_message.starts_with("invalidpassword")
        || error.starts_with("invalidpassword")
}

/// Subset of Keycloak's `UserRepresentation` the facade needs:
///
/// * UUID discovery after `provision_user` — only `id` is required.
/// * `list_users` group-members listing — `id` + profile fields the
///   facade projects into [`IdpUser`].
///
/// `firstName` / `lastName` are concatenated into [`IdpUser::display_name`]
/// per DESIGN "Interactions & Sequences" (KC `firstName + lastName` is the canonical KC profile
/// shape; OIDC `name` claim is not surfaced by the admin REST endpoint).
///
/// `createdTimestamp` is used by the `list_users` cursor sort key
/// `(createdTimestamp ASC, id ASC)` per DESIGN "Interactions & Sequences".
// Field subset of Keycloak's `UserRepresentation`; the derive is the decoder.
#[allow(unknown_lints, de0101_no_serde_in_contract)]
#[domain_model]
#[derive(Deserialize)]
struct UserRep {
    id: Uuid,
    // `default` resolves to 0 (epoch). Real KC always emits `createdTimestamp`;
    // the default exists only for minimal test fixtures. Such users sort
    // before all real users — acceptable for v1 but document if writing new fixtures.
    #[serde(default, rename = "createdTimestamp")]
    created_timestamp: i64,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "firstName")]
    first_name: Option<String>,
    #[serde(default, rename = "lastName")]
    last_name: Option<String>,
}

/// Decoded view of the `IdpUserPagination.cursor` opaque string, as
/// constructed by [`UserFacade::encode_cursor`] and consumed by
/// [`UserFacade::decode_cursor`].
///
/// Wire format is [`toolkit_odata::CursorV1`] base64url-JSON — the same
/// cursor envelope AM's tenant `list_children` flow uses, so `OData`
/// extractor parsing + handler order-recovery (`ODataOrderBy::
/// from_signed_tokens(&c.s)`) work uniformly across the
/// plugin-emitted and AM-native cursor paths (see
/// `account-management/src/api/rest/handlers/users.rs::
/// lower_odata_to_list_users_query`).
///
/// Field mapping into [`CursorV1`]:
///
///   * `k = [last_created_at_ms.to_string(), last_user_id.to_string()]`
///     — composite tiebreaker per DESIGN "Interactions & Sequences" sort
///     `(createdTimestamp ASC, id ASC)`.
///   * `o = SortDir::Asc`, `s = [CURSOR_ORDER_TOKENS]` — pin the
///     hard-coded plugin sort order in the cursor so AM's handler
///     can recover `req.order` on continuation requests via
///     `ODataOrderBy::from_signed_tokens`.
///   * `f = Some(filter_hash)` — short FNV-1a fingerprint of
///     `(tenant_id, realm_name, id_filter, string_filters)`. The
///     `realm_name` fold replaces the pre-CursorV1 explicit
///     `decoded.realm_name != metadata.realm_name` reject — same
///     effective check, now bundled in the hash so a re-binding
///     across pages surfaces as `invalid cursor` through the same
///     path as a filter / tenant change.
///   * `d = "fwd"` — the plugin does not implement backward
///     pagination (it would require fetching from an offset > 0,
///     which v1 forbids).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListUsersCursor {
    filter_hash: String,
    last_created_at: i64,
    last_user_id: Uuid,
}

/// Pre-CursorV1 cursor shape kept ALIVE on the decode side for the
/// rolling-deploy grace window. Mirrors the JSON shape the previous
/// encoder produced: opaque base64url-no-pad of a serde-tagged struct
/// with `realm_name` carried alongside `filter_hash`. Used ONLY by
/// [`UserFacade::decode_cursor_with_legacy_fallback`]; the encoder
/// always emits the current [`CursorV1`] shape.
///
/// Why keep this around: the rolling rollout window means clients
/// that fetched page 1 BEFORE the deploy and page 2 AFTER the deploy
/// would otherwise see `invalid cursor` mid-walk on a server-issued
/// token. The grace decode accepts both shapes; once all in-flight
/// pagination drains (matter of minutes), the legacy struct can be
/// retired.
// The derive IS the grace-window decoder described above.
#[allow(unknown_lints, de0101_no_serde_in_contract)]
#[domain_model]
#[derive(Debug, Deserialize)]
struct LegacyListUsersCursor {
    realm_name: String,
    filter_hash: String,
    #[serde(default)]
    last_created_at: Option<i64>,
    #[serde(default)]
    last_user_id: Option<Uuid>,
}

/// Pre-CursorV1 `filter_hash` computation. Kept ALIVE for the same
/// rolling-deploy grace window as [`LegacyListUsersCursor`]: a legacy
/// cursor is accepted only if its `filter_hash` re-derives from this
/// function given the current request's `(tenant_id, id_filter)` —
/// i.e. the legacy filter context matches today's request.
///
/// Scope of revalidation: tenant + UUID-typed `id` filter only.
/// Legacy cursors were minted before the string-filter predicate
/// ([`StringMatcher`]) was folded into the hash; the encoded hash
/// never folded `realm_name` or
/// `username/email/first_name/last_name/display_name` into its
/// state, so this routine intentionally does not either. Feeding
/// today's [`StringMatcher`] in would change every legacy hash
/// byte-for-byte and reject every legacy cursor minted by the prior
/// release — defeating the rolling-deploy grace window.
///
/// Safety vs. drift: realm rebinding is caught upstream by the
/// explicit `legacy.realm_name != expected_realm` check in
/// [`UserFacade::decode_cursor_with_legacy_fallback`]. A request
/// that adds string filters mid-walk reuses a legacy cursor for at
/// most one more page — the response always emits a fresh `CursorV1`
/// `next_cursor` with the full modern hash, after which the modern
/// equality check guards every subsequent page.
/// FNV-1a 64-bit offset basis / prime — deterministic, non-cryptographic
/// fingerprint (DE0708: no non-FIPS hashers). Public Fowler–Noll–Vo spec with
/// fixed constants → identical output across Rust versions and platforms.
const FNV1A_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01B3;

/// Mix `bytes` into an in-progress FNV-1a 64-bit state.
fn fnv1a_update(state: &mut u64, bytes: impl AsRef<[u8]>) {
    for &b in bytes.as_ref() {
        *state ^= u64::from(b);
        *state = state.wrapping_mul(FNV1A_PRIME);
    }
}

fn legacy_filter_hash(tenant_id: Uuid, user_id_filter: Option<Uuid>) -> String {
    let mut h = FNV1A_BASIS;
    fnv1a_update(&mut h, tenant_id.as_bytes());
    match user_id_filter {
        Some(u) => {
            fnv1a_update(&mut h, [1u8]);
            fnv1a_update(&mut h, u.as_bytes());
        }
        None => {
            fnv1a_update(&mut h, [0u8]);
        }
    }
    format!("{h:016x}")
}

/// Plugin's sort order, hard-coded to `(createdTimestamp ASC, id
/// ASC)` per DESIGN "Interactions & Sequences". The literal token strings here are opaque
/// to KC — they live only in the cursor envelope so [`CursorV1`]'s
/// `o`/`s` fields round-trip correctly through the AM handler's
/// order-recovery step. If the plugin ever forwards `req.order` to
/// the KC sort (the open TODO inside `list_users_impl`), the literals
/// here become inputs to a runtime computation instead of a fixed
/// constant.
const CURSOR_ORDER_TOKENS: &str = "+created_at,+id";
const CURSOR_ORDER_DIR: SortDir = SortDir::Asc;
const CURSOR_DIRECTION: &str = "fwd";

/// Hard cap on the total tenant-group members drained in one
/// `list_users` call. Bounds memory on a pathological membership;
/// reaching it is logged loudly in `list_users_impl` rather than
/// silently truncating. The realm-wide `/users?q=tenant_id:UUID`
/// path is the scale answer past this cap.
const MEMBERS_DRAIN_HARD_CAP: usize = 10_000;

/// String-field match operator lowered from the `OData` `$filter`.
#[domain_model]
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum StringMatchOp {
    /// `eq` — exact, case-sensitive (mirrors KC `exact=true`).
    Eq,
    /// `contains` — substring, case-insensitive (user search).
    Contains,
}

/// Client-side predicate lowered from the typed `OData` `$filter`,
/// evaluated over each KC-fetched member. Mirrors the operator subset
/// the `/users` list contract supports: `and` / `or` of `eq` /
/// `contains` clauses on the string fields (`username` / `email` /
/// `first_name` / `last_name` / `display_name`), and exact typed UUID sets
/// (`id in (...)`) at any supported nesting level.
///
/// `id eq <uuid>` is captured separately by
/// [`UserFacade::extract_id_filter`]; inside a top-level `and` it
/// lowers to [`StringMatcher::Always`] here, and inside an `or` it is
/// rejected (the id half cannot be hoisted out of a disjunction).
///
/// [`StringMatcher::Always`] is the "no string constraint" identity
/// (the no-`$filter` case) and matches every member.
#[domain_model]
#[derive(Default, PartialEq, Eq, Debug, Clone)]
enum StringMatcher {
    #[default]
    Always,
    /// Exact typed ID set, evaluated in place even inside a composite filter.
    IdSet(std::collections::BTreeSet<Uuid>),
    Field {
        field: IdpUserFilterField,
        op: StringMatchOp,
        needle: String,
    },
    All(Vec<StringMatcher>),
    Any(Vec<StringMatcher>),
}

impl StringMatcher {
    /// `true` when `rep` satisfies the predicate. `Always` matches
    /// unconditionally; `All` is conjunction, `Any` disjunction. A
    /// `Field` clause reads the named field off the rep and applies
    /// `op` (case-insensitive substring for `Contains`, exact for
    /// `Eq`); a field absent on the rep never matches.
    fn matches(&self, rep: &UserRep) -> bool {
        match self {
            StringMatcher::Always => true,
            StringMatcher::IdSet(ids) => ids.contains(&rep.id),
            StringMatcher::All(children) => children.iter().all(|c| c.matches(rep)),
            StringMatcher::Any(children) => children.iter().any(|c| c.matches(rep)),
            StringMatcher::Field { field, op, needle } => {
                let actual: Option<String> = match field {
                    IdpUserFilterField::Username => rep.username.clone(),
                    IdpUserFilterField::Email => rep.email.clone(),
                    IdpUserFilterField::FirstName => rep.first_name.clone(),
                    IdpUserFilterField::LastName => rep.last_name.clone(),
                    IdpUserFilterField::DisplayName => UserFacade::user_rep_display_name(rep),
                    // `id` never reaches a Field clause (handled in the
                    // builder); treat defensively as non-matching.
                    IdpUserFilterField::Id => None,
                };
                match (actual, op) {
                    (Some(a), StringMatchOp::Eq) => a == *needle,
                    (Some(a), StringMatchOp::Contains) => {
                        a.to_lowercase().contains(needle.to_lowercase().as_str())
                    }
                    (None, _) => false,
                }
            }
        }
    }

    /// Feed the predicate into the cursor `filter_hash` in a canonical,
    /// collision-resistant shape (variant tag + length-prefixed parts)
    /// so a continuation cursor is only valid for an identical filter.
    fn feed_hash(&self, state: &mut u64) {
        match self {
            StringMatcher::Always => fnv1a_update(state, [0u8]),
            StringMatcher::IdSet(ids) => {
                fnv1a_update(state, [4u8]);
                fnv1a_update(state, ids.len().to_be_bytes());
                for id in ids {
                    fnv1a_update(state, id.as_bytes());
                }
            }
            StringMatcher::Field { field, op, needle } => {
                let field_tag: u8 = match field {
                    IdpUserFilterField::Id => 0,
                    IdpUserFilterField::Username => 1,
                    IdpUserFilterField::Email => 2,
                    IdpUserFilterField::FirstName => 3,
                    IdpUserFilterField::LastName => 4,
                    IdpUserFilterField::DisplayName => 5,
                };
                let op_tag: u8 = match op {
                    StringMatchOp::Eq => 0,
                    StringMatchOp::Contains => 1,
                };
                fnv1a_update(state, [1u8]);
                fnv1a_update(state, [field_tag]);
                fnv1a_update(state, [op_tag]);
                fnv1a_update(state, needle.len().to_be_bytes());
                fnv1a_update(state, needle.as_bytes());
            }
            StringMatcher::All(children) => {
                fnv1a_update(state, [2u8]);
                fnv1a_update(state, children.len().to_be_bytes());
                for c in children {
                    c.feed_hash(state);
                }
            }
            StringMatcher::Any(children) => {
                fnv1a_update(state, [3u8]);
                fnv1a_update(state, children.len().to_be_bytes());
                for c in children {
                    c.feed_hash(state);
                }
            }
        }
    }
}

#[domain_model]
pub struct UserFacade {
    cfg: UserFacadeConfig,
    realm_admin_cfg: RealmAdminConfig,
    kc_factory: Arc<KeycloakAdminClientFactory>,
    codec: Arc<MetadataCodec>,
    // Segregated metric ports (DESIGN "Observability"). One `Arc` per subdomain.
    user_metrics: Arc<dyn UserOpMetricsPort>,
    metadata_metrics: Arc<dyn MetadataCodecMetricsPort>,
    failure_metrics: Arc<dyn FailureMetricsPort>,
}

impl UserFacade {
    #[must_use]
    pub fn new(
        cfg: UserFacadeConfig,
        realm_admin_cfg: RealmAdminConfig,
        kc_factory: Arc<KeycloakAdminClientFactory>,
        codec: Arc<MetadataCodec>,
        user_metrics: Arc<dyn UserOpMetricsPort>,
        metadata_metrics: Arc<dyn MetadataCodecMetricsPort>,
        failure_metrics: Arc<dyn FailureMetricsPort>,
    ) -> Self {
        Self {
            cfg,
            realm_admin_cfg,
            kc_factory,
            codec,
            user_metrics,
            metadata_metrics,
            failure_metrics,
        }
    }

    /// Provision a user in the tenant's bound realm (DESIGN "Interactions & Sequences").
    ///
    /// 1. Decode tenant metadata.
    /// 2. Resolve realm-admin secret source (env for default-shared,
    ///    `OpenBao` [`SecretRef`] otherwise — DESIGN "Domain Model").
    /// 3. `POST /admin/realms/{realm}/users` with `requiredActions` from
    ///    config.
    /// 4. Discover the user's UUID via
    ///    `GET .../users?username=...&exact=true` (defensive — sibling Phase
    ///    10 client-create uses the same pattern instead of parsing
    ///    `Location`).
    /// 5. Add the user to the tenant group via
    ///    `PUT .../users/{uuid}/groups/{tenant_group_id}`.
    ///
    /// # Errors
    ///
    /// * [`PluginError::UserOpRejected`] — malformed metadata, KC 409 on
    ///   user-create (duplicate), or KC 404 on group-add (tenant group
    ///   disappeared).
    /// * [`PluginError::UserOpUnavailable`] — KC 5xx / transport / timeout
    ///   on any step (re-tagged from [`PluginError::KcRest`] by
    ///   [`user_translate_kc`]).
    /// * [`PluginError::CredStoreRead`] — realm-admin credential resolution
    ///   failure from `OpenBao` (Adopted / Created modes; Phase 15 maps this
    ///   via the same boundary table as the tenant-side facade).
    pub async fn provision_user_inner(
        &self,
        ctx: &SecurityContext,
        req: &IdpProvisionUserRequest,
    ) -> Result<IdpUser, PluginError> {
        let started = Instant::now();
        let result = self.provision_user_impl(req).await;
        let elapsed = started.elapsed().as_secs_f64();
        self.user_metrics
            .user_op_duration(UserOp::ProvisionUser, elapsed);
        match &result {
            Ok((user, realm_name, realm_binding)) => {
                audit::emit_user_provisioned(
                    ctx,
                    req.tenant_context.tenant_id,
                    realm_name,
                    *realm_binding,
                    user.id,
                    &user.username,
                );
            }
            Err(e) => {
                self.record_user_op_failure(PluginOp::ProvisionUser, e);
            }
        }
        result.map(|(user, _, _)| user)
    }

    /// Inner implementation returning the [`IdpUser`] plus the bound
    /// realm name / binding so the outer wrapper can emit a fully-labelled
    /// `user.provisioned` audit event without re-decoding the metadata.
    #[allow(
        clippy::cognitive_complexity,
        reason = "flat provisioning sequence (decode metadata -> resolve realm -> create user -> set credential -> compensate on failure) is the ordering reviewers verify against DESIGN 5.2; extracting steps would scatter the compensation path"
    )]
    async fn provision_user_impl(
        &self,
        req: &IdpProvisionUserRequest,
    ) -> Result<(IdpUser, String, RealmBinding), PluginError> {
        // ---- Step 1: decode metadata. ----
        let metadata = self.decode_tenant_metadata(req.tenant_context.metadata.as_ref())?;

        // ---- Step 2: resolve realm-admin secret source. ----
        let secret_source = self.build_secret_source(&metadata)?;

        // ---- Step 4: create user in target realm. ----
        //
        // Each KC step is wrapped INDIVIDUALLY in `with_reactive_401`
        // (not as a single block) because `create_user` is the only
        // non-idempotent step in the chain: a block-level retry would
        // re-issue the POST on the 2nd pass and trip KC's 409
        // duplicate-username branch, surfacing as a phantom
        // `UserOpRejected` to AM. Per-call wrapping keeps each retry
        // narrowly scoped to the call whose token was rejected.
        self.kc_factory
            .with_reactive_401(
                &metadata.realm_name,
                &metadata.admin_client_id,
                &secret_source,
                |realm_admin| {
                    // Clone-in-closure: the `Fn` bound requires the
                    // closure to be callable multiple times (the
                    // wrapper may invoke it twice on a 401 retry), so
                    // owned-`String` captures must clone here rather
                    // than be moved by the inner `async move`.
                    let realm_name = metadata.realm_name.clone();
                    async move { self.create_user(&realm_admin, &realm_name, req).await }
                },
            )
            .await
            .map_err(user_translate_kc)?;

        // ---- Step 5: discover the new user's UUID. ----
        let user_uuid = self
            .kc_factory
            .with_reactive_401(
                &metadata.realm_name,
                &metadata.admin_client_id,
                &secret_source,
                |realm_admin| {
                    let realm_name = metadata.realm_name.clone();
                    let username = req.payload.username.clone();
                    async move {
                        self.lookup_user_uuid(&realm_admin, &realm_name, &username)
                            .await
                    }
                },
            )
            .await
            .map_err(user_translate_kc)?;

        // ---- Step 6: add user to tenant group. ----
        // If the group-add fails after the user was just created, the
        // user is orphaned in KC with no tenant binding. Best-effort
        // delete the user before propagating the error so AM doesn't
        // leave a permanent dangling identity behind. Delete failure
        // is logged but doesn't override the original group-add error
        // — operators see the primary failure first.
        let add_result = self
            .kc_factory
            .with_reactive_401(
                &metadata.realm_name,
                &metadata.admin_client_id,
                &secret_source,
                |realm_admin| {
                    let realm_name = metadata.realm_name.clone();
                    let tenant_group_id = metadata.tenant_group_id;
                    async move {
                        self.add_user_to_group(
                            &realm_admin,
                            &realm_name,
                            user_uuid,
                            tenant_group_id,
                        )
                        .await
                    }
                },
            )
            .await
            .map_err(user_translate_kc);
        if let Err(group_err) = add_result {
            // Compensation: delete the orphan user. Same reactive-401
            // wrap so a rotated secret doesn't break the compensation
            // path (which would leave a permanent dangling identity).
            let compensation = self
                .kc_factory
                .with_reactive_401(
                    &metadata.realm_name,
                    &metadata.admin_client_id,
                    &secret_source,
                    |realm_admin| {
                        let realm_name = metadata.realm_name.clone();
                        async move { self.delete_user(&realm_admin, &realm_name, user_uuid).await }
                    },
                )
                .await
                .map_err(user_translate_kc);
            if let Err(compensation_err) = compensation {
                tracing::warn!(
                    target: "keycloak_idp_plugin.user_facade",
                    user_uuid = %user_uuid,
                    realm = %metadata.realm_name,
                    primary_error = %group_err,
                    compensation_error = %compensation_err,
                    "orphan-user compensation FAILED: KC user remains without tenant binding",
                );
                // Emit the actionable counter so dashboards alert on
                // dangling KC users — `tracing::warn!` alone isn't a
                // metric and operators have no way to graph it.
                self.user_metrics
                    .orphan_user_compensation(OrphanCompensationOutcome::Failed);
            } else {
                tracing::info!(
                    target: "keycloak_idp_plugin.user_facade",
                    user_uuid = %user_uuid,
                    realm = %metadata.realm_name,
                    primary_error = %group_err,
                    "orphan-user compensation OK: deleted KC user after add_user_to_group failure",
                );
                self.user_metrics
                    .orphan_user_compensation(OrphanCompensationOutcome::Ok);
            }
            return Err(group_err);
        }

        // ---- Step 7: build the projection. ----
        let mut user = IdpUser::new(user_uuid, &req.payload.username);
        if let Some(email) = req.payload.email.as_ref() {
            user = user.with_email(email);
        }
        if let Some(display_name) = req.payload.display_name.as_ref() {
            user = user.with_display_name(display_name);
        }
        Ok((user, metadata.realm_name, metadata.realm_binding))
    }

    /// Shared failure-recording helper for user-op sites. Increments both
    /// the generic `keycloak_idp_plugin_failure_total` counter and (when the
    /// underlying error is a metadata-decode failure) the dedicated
    /// `keycloak_idp_plugin_metadata_decode_failure_total` trigger metric.
    fn record_user_op_failure(&self, op: PluginOp, e: &PluginError) {
        if let PluginError::MetadataDecode(de) = e {
            self.metadata_metrics
                .metadata_decode_failure(VersionObserved::from(de));
        }
        self.failure_metrics.failure(op, FailureVariant::from(e));
    }

    /// Decode the persisted [`TenantIdpMetadataV1`] from an optional metadata
    /// blob. A missing or malformed blob is a user-op rejection (the tenant
    /// must exist and be bound before users can be provisioned into it).
    ///
    /// All three user-op entry points (`provision_user_impl`,
    /// `deprovision_user_impl`, `list_users_impl`) share this single decode
    /// step so error messages and error variant stay consistent.
    ///
    /// **Variant preservation**: a malformed blob surfaces as
    /// [`PluginError::MetadataDecode`] (not `UserOpRejected`), so
    /// [`record_user_op_failure`]'s dedicated
    /// `metadata_decode_failure` counter fires with the right
    /// `version_observed` label on the user-side path symmetric with
    /// the tenant-side. The SDK boundary in `idp_impl.rs` translates
    /// `MetadataDecode` to `IdpUserOperationFailure::Rejected`, so
    /// AM sees the same `Rejected` shape as before — but operators
    /// now keep the per-decode-variant telemetry.
    ///
    /// [`record_user_op_failure`]: Self::record_user_op_failure
    fn decode_tenant_metadata(
        &self,
        blob: Option<&serde_json::Value>,
    ) -> Result<TenantIdpMetadataV1, PluginError> {
        match self.codec.decode(blob.cloned()) {
            Ok(Some(m)) => Ok(m),
            Ok(None) => Err(PluginError::UserOpRejected {
                detail: "metadata decode failed: tenant has no persisted IdP metadata".into(),
            }),
            // Keep the typed `DecodeError` so the dedicated decode-
            // failure counter can be incremented at the failure-
            // recording seam. The Phase 15 SDK boundary translates
            // `MetadataDecode` → `IdpUserOperationFailure::Rejected`
            // so AM sees no behaviour change.
            Err(e) => Err(PluginError::MetadataDecode(e)),
        }
    }

    /// DESIGN "Domain Model" selection: Shared blobs are persisted with
    /// `admin_secret_ref = None` and resolve via the env-backed inline
    /// secret (the `${VAR}` placeholder is expanded at module init via
    /// toolkit's `ExpandVars`); Adopted / Created always carry a templated
    /// `OpenBao` [`SecretRef`] on the metadata blob.
    fn build_secret_source(
        &self,
        metadata: &TenantIdpMetadataV1,
    ) -> Result<SecretSource, PluginError> {
        if let Some(ref_str) = metadata.admin_secret_ref.as_ref() {
            let secret_ref = SecretRef::new(ref_str).map_err(|e| PluginError::UserOpRejected {
                detail: format!("admin_secret_ref invalid: {e}"),
            })?;
            Ok(SecretSource::OpenBaoRef(secret_ref))
        } else {
            Ok(SecretSource::Inline(
                self.realm_admin_cfg
                    .default_shared_realm_secret
                    .clone_into_secret_string(),
            ))
        }
    }

    /// `POST /admin/realms/{realm}/users` with the canonical create-user
    /// payload (DESIGN "Interactions & Sequences"). Returns success on any 2xx. Every 409 is
    /// [`PluginError::UserOpDuplicate`] — the status itself is the
    /// uniqueness signal; KC's `errorMessage` only refines the colliding
    /// field. A 400/422 carrying a KC password-policy reject
    /// becomes [`PluginError::UserOpPasswordPolicy`], any other 400/422
    /// is a terminal [`PluginError::UserOpRejected`]; 5xx / transport /
    /// timeout re-tag as [`PluginError::UserOpUnavailable`] via
    /// [`user_translate_kc`].
    async fn create_user(
        &self,
        realm_admin: &KcAdminClient,
        realm_name: &str,
        req: &IdpProvisionUserRequest,
    ) -> Result<(), PluginError> {
        let url = format!(
            "{}admin/realms/{}/users",
            self.kc_factory.base_url_str(),
            encode_component(realm_name),
        );
        let mut body = serde_json::Map::new();
        body.insert(
            "username".into(),
            serde_json::Value::String(req.payload.username.clone()),
        );
        body.insert("enabled".into(), serde_json::Value::Bool(true));
        // Tenant-scoped users always get `user_type=user`. The realm protocol
        // mapper consumes this attribute into the `user_type` JWT claim that
        // the AuthN plugin propagates into the SecurityContext. Platform /
        // system identities have separate provisioning paths and set their
        // own `user_type` outside this facade.
        body.insert(
            "attributes".into(),
            serde_json::json!({
                "tenant_id": [req.tenant_context.tenant_id.to_string()],
                "user_type": ["user"],
            }),
        );
        // Required actions: start from the plugin-level default and append
        // UPDATE_PASSWORD when the caller supplied a temporary password. This
        // preserves operator-configured baseline (e.g. VERIFY_EMAIL in prod)
        // while still honouring the per-request temporary-credential signal.
        let mut required_actions: Vec<String> = self.cfg.default_user_required_actions.clone();
        if let Some(pw) = req.payload.password.as_ref()
            && pw.temporary
            && !required_actions.iter().any(|a| a == "UPDATE_PASSWORD")
        {
            required_actions.push("UPDATE_PASSWORD".to_owned());
        }
        // `emailVerified` is derived from `required_actions` so the two stay
        // coupled: KC silently skips the `VERIFY_EMAIL` action when the user
        // is already marked verified. When the operator opts users into
        // VERIFY_EMAIL we send `emailVerified=false` so the verify-email
        // flow actually gates token issuance; otherwise AM is treated as
        // authoritative for the address and the user is immediately usable
        // for the password grant. Single source of truth prevents the
        // classic misconfiguration where `[VERIFY_EMAIL]` is set but the
        // pre-verified flag silently nullifies it.
        let email_verified = !required_actions.iter().any(|a| a == "VERIFY_EMAIL");
        body.insert(
            "emailVerified".into(),
            serde_json::Value::Bool(email_verified),
        );
        body.insert(
            "requiredActions".into(),
            serde_json::json!(required_actions),
        );
        if let Some(email) = req.payload.email.as_ref() {
            body.insert("email".into(), serde_json::Value::String(email.clone()));
        }
        if let Some(first_name) = req.payload.first_name.as_ref() {
            body.insert(
                "firstName".into(),
                serde_json::Value::String(first_name.clone()),
            );
        }
        if let Some(last_name) = req.payload.last_name.as_ref() {
            body.insert(
                "lastName".into(),
                serde_json::Value::String(last_name.clone()),
            );
        }
        if let Some(pw) = req.payload.password.as_ref() {
            // Embed the initial credential in the same POST so the user is
            // immediately usable for password grant. Keycloak accepts a
            // credentials array on user-create; `temporary: false` makes the
            // password permanent, `true` flips the required-actions branch
            // above. The plaintext value lives only on this request's stack
            // and the outbound HTTPS body — AM never persists it.
            body.insert(
                "credentials".into(),
                serde_json::json!([
                    {
                        "type": "password",
                        "value": pw.value,
                        "temporary": pw.temporary,
                    }
                ]),
            );
        }
        let body = serde_json::Value::Object(body);
        let (status, body_text) = realm_admin
            .raw_request("POST", &url, Some(&body), "/admin/realms/{realm}/users")
            .await
            .map_err(user_translate_kc)?;
        if (200..=299).contains(&status) {
            return Ok(());
        }
        if status == 401 {
            // Surface to the enclosing reactive-401 wrapper for rotation
            // recovery. Auth-layer rejection → no user created → safe
            // for the wrapper's exactly-once retry contract.
            return Err(PluginError::KcRest {
                method: "POST",
                path_template: "/admin/realms/{realm}/users".into(),
                status: KcStatusKind::Http(401),
                body_first_2kb: redact_secrets(&truncate_2kb(&body_text)),
            });
        }
        let redacted = redact_secrets(&truncate_2kb(&body_text));
        if status == 409 {
            // on this endpoint a 409 has no cause other than a
            // uniqueness collision, so the STATUS is the classification —
            // every 409 rides the typed duplicate lane and stays HTTP 409
            // at the AM boundary regardless of KC wording drift; the body
            // only refines which field collided (unattributable bodies →
            // `UsernameOrEmail`).
            return Err(PluginError::UserOpDuplicate {
                field: classify_kc_conflict(&body_text),
                detail: format!(
                    "user already exists: kc:POST /admin/realms/{{realm}}/users -> 409: {redacted}"
                ),
            });
        }
        // KC's payload-rejection statuses. A recognised
        // password-policy reject surfaces the structured `password` /
        // `PASSWORD_POLICY` violation; every other 400/422 (invalid email
        // format, user-profile attribute validation, malformed payload) is
        // a terminal rejection — previously these fell through to the
        // catch-all below and mis-surfaced as a retryable 503.
        if status == 400 || status == 422 {
            if is_kc_password_policy_reject(&body_text) {
                return Err(PluginError::UserOpPasswordPolicy {
                    detail: format!(
                        "password policy rejected: kc:POST /admin/realms/{{realm}}/users -> \
                         {status}: {redacted}"
                    ),
                });
            }
            return Err(PluginError::UserOpRejected {
                detail: format!("kc:POST /admin/realms/{{realm}}/users -> {status}: {redacted}"),
            });
        }
        // Everything else (5xx, transport oddities, 403/404 realm-level
        // problems) keeps the unavailable lane: not a payload rejection,
        // and a retry after operator intervention can succeed.
        Err(PluginError::UserOpUnavailable {
            detail: format!("kc:POST /admin/realms/{{realm}}/users -> {status}: {redacted}"),
        })
    }

    /// `GET /admin/realms/{realm}/users?username={u}&exact=true` — defensive
    /// UUID discovery to avoid the `Location`-header parse Keycloak returns
    /// on `POST /users` (Phase 10's `lookup_client_uuid` uses the same
    /// pattern). Empty result → [`PluginError::UserOpUnavailable`] (the
    /// user was created moments ago; this is a KC-side consistency
    /// anomaly, not a payload rejection).
    async fn lookup_user_uuid(
        &self,
        realm_admin: &KcAdminClient,
        realm_name: &str,
        username: &str,
    ) -> Result<Uuid, PluginError> {
        let url = format!(
            "{}admin/realms/{}/users?username={}&exact=true",
            self.kc_factory.base_url_str(),
            encode_component(realm_name),
            encode_component(username),
        );
        let (status, body_text) = realm_admin
            .raw_request("GET", &url, None, "/admin/realms/{realm}/users")
            .await
            .map_err(user_translate_kc)?;
        if status == 401 {
            // Surface 401 verbatim so the enclosing reactive-401 wrapper
            // re-acquires + retries against a fresh bearer.
            return Err(PluginError::KcRest {
                method: "GET",
                path_template: "/admin/realms/{realm}/users".into(),
                status: KcStatusKind::Http(401),
                body_first_2kb: redact_secrets(&truncate_2kb(&body_text)),
            });
        }
        if !(200..=299).contains(&status) {
            return Err(PluginError::UserOpUnavailable {
                detail: format!(
                    "kc:GET /admin/realms/{{realm}}/users -> {status}: {}",
                    redact_secrets(&truncate_2kb(&body_text)),
                ),
            });
        }
        let users: Vec<UserRep> =
            serde_json::from_str(&body_text).map_err(|e| PluginError::UserOpUnavailable {
                detail: format!("kc:GET /admin/realms/{{realm}}/users json decode: {e}"),
            })?;
        users
            .into_iter()
            .next()
            .map(|u| u.id)
            .ok_or_else(|| PluginError::UserOpUnavailable {
                detail: format!(
                    "kc:GET /admin/realms/{{realm}}/users -> 200: username '{username}' not found after create",
                ),
            })
    }

    /// `PUT /admin/realms/{realm}/users/{user_uuid}/groups/{tenant_group_id}`
    /// — idempotent membership add. 2xx → success. 404 → tenant group
    /// disappeared (operator/concurrent delete) — surface as
    /// [`PluginError::UserOpRejected`] so AM treats the call as a payload
    /// rejection rather than a transient outage. 5xx / transport →
    /// [`PluginError::UserOpUnavailable`] via [`user_translate_kc`].
    async fn add_user_to_group(
        &self,
        realm_admin: &KcAdminClient,
        realm_name: &str,
        user_uuid: Uuid,
        tenant_group_id: Uuid,
    ) -> Result<(), PluginError> {
        let url = format!(
            "{}admin/realms/{}/users/{}/groups/{}",
            self.kc_factory.base_url_str(),
            encode_component(realm_name),
            user_uuid,
            tenant_group_id,
        );
        let (status, body_text) = realm_admin
            .raw_request(
                "PUT",
                &url,
                None,
                "/admin/realms/{realm}/users/{user_uuid}/groups/{tenant_group_id}",
            )
            .await
            .map_err(user_translate_kc)?;
        if (200..=299).contains(&status) {
            return Ok(());
        }
        if status == 401 {
            return Err(PluginError::KcRest {
                method: "PUT",
                path_template: "/admin/realms/{realm}/users/{user_uuid}/groups/{tenant_group_id}"
                    .into(),
                status: KcStatusKind::Http(401),
                body_first_2kb: redact_secrets(&truncate_2kb(&body_text)),
            });
        }
        if status == 404 {
            return Err(PluginError::UserOpRejected {
                detail: format!(
                    "tenant group disappeared: kc:PUT /admin/realms/{{realm}}/users/{{user_uuid}}/groups/{{tenant_group_id}} -> 404: {}",
                    redact_secrets(&truncate_2kb(&body_text)),
                ),
            });
        }
        Err(PluginError::UserOpUnavailable {
            detail: format!(
                "kc:PUT /admin/realms/{{realm}}/users/{{user_uuid}}/groups/{{tenant_group_id}} -> {status}: {}",
                redact_secrets(&truncate_2kb(&body_text)),
            ),
        })
    }

    /// Deprovision a user from the tenant's bound realm (DESIGN "Interactions &
    /// Sequences" and "API Contracts").
    ///
    /// 1. Decode tenant metadata.
    /// 2. Resolve realm-admin secret source (env vs `OpenBaoRef`).
    /// 3. *(Optional)* `POST /admin/realms/{realm}/users/{id}/logout` to
    ///    revoke active sessions when
    ///    [`UserFacadeConfig::session_revoke_on_deprovision`] is `true`.
    ///    Logout is best-effort: 2xx and 404 both proceed; transport /
    ///    5xx are logged + ignored — the delete is the primary effect
    ///    per the DESIGN trait-doc contract.
    /// 4. `DELETE /admin/realms/{realm}/users/{user_id}`:
    ///    - 2xx → `Ok(())`.
    ///    - 404 / 410 → `Ok(())` (idempotency contract — DESIGN "Interactions & Sequences").
    ///    - 5xx / transport / timeout → [`PluginError::UserOpUnavailable`].
    ///
    /// # Errors
    ///
    /// * [`PluginError::UserOpRejected`] — malformed metadata.
    /// * [`PluginError::UserOpUnavailable`] — KC 5xx / transport /
    ///   timeout on the delete step (re-tagged from
    ///   [`PluginError::KcRest`] by [`user_translate_kc`]).
    pub async fn deprovision_user_inner(
        &self,
        ctx: &SecurityContext,
        req: &IdpDeprovisionUserRequest,
    ) -> Result<(), PluginError> {
        let started = Instant::now();
        let result = self.deprovision_user_impl(req).await;
        let elapsed = started.elapsed().as_secs_f64();
        self.user_metrics
            .user_op_duration(UserOp::DeprovisionUser, elapsed);
        match &result {
            Ok((realm_name, realm_binding)) => {
                audit::emit_user_deprovisioned(
                    ctx,
                    req.tenant_context.tenant_id,
                    realm_name,
                    *realm_binding,
                    req.user_id,
                    // The DELETE step folds 404/410 into success per the
                    // idempotency contract — we don't currently distinguish
                    // the two at this layer, so `already_absent = false`
                    // for every Ok path. (DESIGN "Audit and Metrics Boundary" marks this as a
                    // future-precision target.)
                    /* already_absent = */
                    false,
                );
            }
            Err(e) => {
                self.record_user_op_failure(PluginOp::DeprovisionUser, e);
            }
        }
        result.map(|_| ())
    }

    /// Inner implementation returning the bound realm name / binding so
    /// the outer wrapper can emit a fully-labelled `user.deprovisioned`
    /// audit event without re-decoding the metadata.
    async fn deprovision_user_impl(
        &self,
        req: &IdpDeprovisionUserRequest,
    ) -> Result<(String, RealmBinding), PluginError> {
        // ---- Step 1: decode metadata. ----
        let metadata = self.decode_tenant_metadata(req.tenant_context.metadata.as_ref())?;

        // ---- Step 2: resolve realm-admin secret source. ----
        let secret_source = self.build_secret_source(&metadata)?;

        // ---- Step 2.5: binding check before any destructive call. ----
        //
        // Refuse to drive the delete unless the user's stored
        // `tenant_id` attribute (set by `create_user` per `ADR-0006`) matches
        // the request's `tenant_context.tenant_id`. Without this check the
        // handler is willing to delete any KC user by `user_id` from any
        // tenant path — the AuthZ-emitted SQL constraint
        // (`OWNER_TENANT_ID IN [caller_tid]`) clamps AM-DB writes, but
        // users live in KC, so the SQL clamp never sees this path. The
        // binding check is therefore the only defence against
        // cross-tenant user deletion.
        //
        // A mismatch (or a missing attribute) is collapsed onto the
        // idempotent-absent success contract documented in
        // `cpt-cf-account-management-fr-idp-user-deprovision`: the call
        // returns the same shape a "user already gone" path would, so the
        // caller cannot distinguish "wrong tenant" from "already
        // deprovisioned". A tracing WARN under
        // `keycloak_idp_plugin.user_binding` records the rejection for
        // operator visibility without leaking the discriminator over the
        // wire.
        let expected_tid = req.tenant_context.tenant_id;
        let actual_tid = self
            .kc_factory
            .with_reactive_401(
                &metadata.realm_name,
                &metadata.admin_client_id,
                &secret_source,
                |realm_admin| {
                    let realm_name = metadata.realm_name.clone();
                    async move {
                        self.read_user_owner_tenant(&realm_admin, &realm_name, req.user_id)
                            .await
                    }
                },
            )
            .await
            .map_err(user_translate_kc)?;
        if actual_tid != Some(expected_tid) {
            tracing::warn!(
                target: "keycloak_idp_plugin.user_binding",
                user_id = %req.user_id,
                expected_tenant_id = %expected_tid,
                actual_tenant_id = ?actual_tid,
                realm_name = %metadata.realm_name,
                "user deprovision skipped: stored tenant_id attribute does not match request - cross-tenant DELETE prevented"
            );
            // Idempotent-absent return: skip revoke_sessions + delete_user
            // entirely. AM observes `Ok(())` and surfaces HTTP 204 No
            // Content to the caller, identical to the genuine
            // already-absent path.
            return Ok((metadata.realm_name, metadata.realm_binding));
        }

        // ---- Steps 3-5: realm-admin work wrapped in reactive-401.
        // Both `revoke_sessions` (idempotent best-effort) and
        // `delete_user` (idempotent: KC vendor 404/410 → ok) tolerate
        // exactly-once retry, so a single closure wrap is safe even
        // if the 401 fires on `delete_user` after `revoke_sessions`
        // already succeeded.
        //
        // `realm_name` is cloned INSIDE the outer closure (not in the
        // inner `async move`) so the `Fn` bound on `with_reactive_401`
        // is satisfied — the wrapper may invoke the closure twice on a
        // 401 retry, and each invocation needs its own owned copy.
        self.kc_factory
            .with_reactive_401(
                &metadata.realm_name,
                &metadata.admin_client_id,
                &secret_source,
                |realm_admin| {
                    let realm_name = metadata.realm_name.clone();
                    async move {
                        if self.cfg.session_revoke_on_deprovision {
                            self.revoke_sessions(&realm_admin, &realm_name, req.user_id)
                                .await;
                        }
                        self.delete_user(&realm_admin, &realm_name, req.user_id)
                            .await
                    }
                },
            )
            .await
            .map_err(user_translate_kc)?;
        Ok((metadata.realm_name, metadata.realm_binding))
    }

    /// List users from the tenant's bound realm (DESIGN "Interactions & Sequences").
    ///
    /// 1. Decode tenant metadata.
    /// 2. Resolve realm-admin secret source.
    /// 3. Decode the opaque cursor (if any) and validate that its
    ///    `realm_name` + `filter_hash` still match this request —
    ///    mismatch → [`PluginError::UserOpRejected`] ("invalid cursor").
    /// 4. Resolve effective page limit:
    ///    `limit = min(req.pagination.top(), max)`. (`top()` is already
    ///    `>= 1` and `<= MAX_TOP` per SDK constructor invariants; the
    ///    facade's own `list_users_page_limit_max` is the stricter
    ///    cap.)
    /// 5. `GET /admin/realms/{realm}/groups/{tenant_group_id}/members?first=0&max={limit+1}`.
    /// 6. Sort response `(createdTimestamp ASC, id ASC)` client-side.
    /// 7. Skip entries already returned on prior pages (cursor tiebreaker).
    /// 8. Apply optional `user_id_filter` post-skip.
    /// 9. Emit first `limit` items; encode `(last_created_at, last_user_id)`
    ///    of the last-emitted user as `next_cursor` when `limit+1` results
    ///    were present after the skip step.
    ///
    /// # Errors
    ///
    /// * [`PluginError::UserOpRejected`] — malformed metadata or
    ///   mismatched cursor.
    /// * [`PluginError::UserOpUnavailable`] — KC 4xx (non-404 group
    ///   gone) / 5xx / transport / timeout on the members fetch.
    pub async fn list_users_inner(
        &self,
        _ctx: &SecurityContext, // list_users emits no audit event (read-only) — DESIGN "Audit and Metrics Boundary"
        req: &IdpListUsersRequest,
    ) -> Result<Page<IdpUser>, PluginError> {
        let started = Instant::now();
        let result = self.list_users_impl(req).await;
        let elapsed = started.elapsed().as_secs_f64();
        self.user_metrics
            .user_op_duration(UserOp::ListUsers, elapsed);
        if let Err(e) = &result {
            self.record_user_op_failure(PluginOp::ListUsers, e);
        }
        result
    }

    // TODO(admin-bind follow-up): the `order` clause is still dropped in
    // favour of the hard-coded `(createdTimestamp ASC, id ASC)` cursor
    // sort, and the v1 client-side filter strategy drains bounded
    // pages from KC's `/groups/{tenant_group_id}/members` endpoint
    // (which has no search params) and post-filters. That is correct
    // for small/medium tenants but doesn't scale to "find one user in
    // a tenant with 100k members". Larger follow-up: (a) switch to
    // KC's realm-wide `/users?username=…&q=tenant_id:<uuid>&exact=true`
    // when string filters are present so KC narrows server-side; (b)
    // forward `req.order` to the chosen sort key (timestamp /
    // username / email); (c) widen the test matrix across
    // field×op×order×pagination shapes. The wire-contract bug
    // (string-field filters returning the full user set) is closed by
    // [`Self::build_string_matcher`] + the post-id-filter retain
    // below. Typed UUID `id in (...)` supports batch lookup; other
    // `InList`/`not`/`ne`/`startswith`/`endswith`/invalid literal
    // shapes (and `id eq` inside `or`) surface `UnsupportedOperation`
    // (HTTP 501) instead of silently dropping the filter clause.
    async fn list_users_impl(
        &self,
        req: &IdpListUsersRequest,
    ) -> Result<Page<IdpUser>, PluginError> {
        // ---- Step 1: decode metadata. ----
        let metadata = self.decode_tenant_metadata(req.tenant_context.metadata.as_ref())?;

        // ---- Step 2: resolve realm-admin secret source. ----
        let secret_source = self.build_secret_source(&metadata)?;

        // ---- Step 3: decode + validate the inbound cursor (if any). ----
        let tenant_id = req.tenant_context.tenant_id;
        let id_filter = Self::extract_id_filter(req.filter.as_ref());
        // Lower string clauses and typed UUID ID sets into a predicate.
        // Unsupported operators (`ne`, non-ID `in`,
        // `not`, `startswith`, …, or `id eq` inside `or`) surface as
        // `UnsupportedOperation` → 501 BEFORE any KC call, distinct from
        // a 200-with-unfiltered-set false-success.
        let string_matcher = Self::build_string_matcher(req.filter.as_ref())?;
        let filter_hash =
            Self::filter_hash(tenant_id, &metadata.realm_name, id_filter, &string_matcher);
        // Decode the optional inbound cursor + verify its filter_hash
        // matches the current request's filter shape. The hash folds in
        // `realm_name`, so a realm-rebinding mid-pagination surfaces
        // here as `invalid cursor` (replaces the pre-CursorV1 explicit
        // `decoded.realm_name != metadata.realm_name` reject).
        let (last_created_at_param, last_user_id_param) = match req.pagination.cursor() {
            Some(c) => {
                // `decode_cursor_with_legacy_fallback` validates the
                // cursor against the current request's filter context
                // internally — both for the modern CursorV1 shape AND
                // the legacy pre-CursorV1 shape accepted during the
                // rolling-deploy grace window. Caller no longer
                // compares hashes here.
                let (created_at, user_id) = Self::decode_cursor_with_legacy_fallback(
                    c,
                    &metadata.realm_name,
                    tenant_id,
                    id_filter,
                    &filter_hash,
                )?;
                (Some(created_at), Some(user_id))
            }
            None => (None, None),
        };

        // ---- Step 4: resolve effective page limit. ----
        // `top()` is already bounded by `IdpUserPagination::MAX_TOP` (200)
        // per the SDK constructor; the facade's `list_users_page_limit_max`
        // can further tighten the cap (operator policy).
        let limit = req.pagination.top().min(self.cfg.list_users_page_limit_max);

        // ---- Step 5: drain the FULL group membership, then sort / skip /
        //              filter client-side (reactive-401 wrapped). ----
        //
        // `/admin/realms/{realm}/groups/{tenant_group_id}/members` has no
        // server-side sort or `q=`-filter knobs, so the facade pages the
        // ENTIRE membership into memory and applies the DESIGN "Interactions & Sequences" sort
        // key `(createdTimestamp ASC, id ASC)` + cursor-skip + filters
        // itself. The loop drains every KC page (`first += page_size`)
        // until a short page signals the end.
        //
        // The pre-fix code fetched a single `first = 0` window of
        // `list_users_page_limit_max + 1` rows, which silently hid every
        // member KC ordered past that window (KC orders group members by
        // username). Any tenant with more than one page of members lost
        // users from BOTH the listing and `$filter` — create-then-list
        // returned 201 yet the user never appeared.
        //
        // `MEMBERS_DRAIN_HARD_CAP` bounds the total pulled so a
        // pathological membership cannot pin unbounded memory; reaching it
        // is logged loudly, never silently truncated. The realm-wide
        // `/users?q=tenant_id:UUID` path is the scale answer past that cap.
        //
        // `fetch_group_members` is a pure read, so the exactly-once 401
        // retry `with_reactive_401` performs re-runs the whole drain
        // safely (realm-admin secret rotation recovery without surfacing a
        // transient 401 to AM).
        let page_size = self.cfg.list_users_page_limit_max.max(1);
        let mut members = self
            .kc_factory
            .with_reactive_401(
                &metadata.realm_name,
                &metadata.admin_client_id,
                &secret_source,
                |realm_admin| {
                    let realm_name = metadata.realm_name.clone();
                    let tenant_group_id = metadata.tenant_group_id;
                    async move {
                        let mut all: Vec<UserRep> = Vec::new();
                        let mut offset: u32 = 0;
                        loop {
                            let batch = self
                                .fetch_group_members(
                                    &realm_admin,
                                    &realm_name,
                                    tenant_group_id,
                                    offset,
                                    page_size,
                                )
                                .await?;
                            let got = batch.len();
                            all.extend(batch);
                            // Short page ⇒ the group is fully drained.
                            if got < page_size as usize {
                                break;
                            }
                            // Loud safety valve — see cap rationale above.
                            if all.len() >= MEMBERS_DRAIN_HARD_CAP {
                                tracing::warn!(
                                    target: "keycloak_idp_plugin.user_facade",
                                    tenant_id = %tenant_id,
                                    realm = %realm_name,
                                    drained = all.len(),
                                    cap = MEMBERS_DRAIN_HARD_CAP,
                                    "list_users: tenant group membership exceeds drain \
                                     hard-cap before complete enumeration; refusing an incomplete result"
                                );
                                return Err(PluginError::UserOpUnavailable {
                                    detail: "tenant membership enumeration exceeded its safety limit".into(),
                                });
                            }
                            offset = offset.saturating_add(page_size);
                        }
                        Ok(all)
                    }
                },
            )
            .await
            .map_err(user_translate_kc)?;

        // Defensive sort: KC does not guarantee stable order across pages.
        members.sort_by(|a, b| {
            a.created_timestamp
                .cmp(&b.created_timestamp)
                .then_with(|| a.id.cmp(&b.id))
        });
        // Offset paging over that unstable order can re-observe a row that
        // shifted across a page boundary during the drain. Equal-id rows
        // are adjacent after the sort above (same user ⇒ same timestamp),
        // so drop adjacent duplicates by id to keep a user from being
        // double-counted in the page or the cursor math.
        members.dedup_by(|a, b| a.id == b.id);

        // Skip entries already returned on prior pages (cursor tiebreaker).
        if let (Some(t_cur), Some(id_cur)) = (last_created_at_param, last_user_id_param) {
            members.retain(|u| (u.created_timestamp, u.id) > (t_cur, id_cur));
        }

        // Apply optional id-eq filter post-skip.
        members.retain(|u| id_filter.is_none_or(|wanted| u.id == wanted));
        // Apply the string-field predicate (`eq` / `contains` / `and` /
        // `or`) lowered from `$filter`. `StringMatcher::Always` (the
        // no-`$filter` case) matches every row, so this is a no-op then.
        members.retain(|u| string_matcher.matches(u));

        // `has_more` is computed AFTER cursor-skip + filter-retain so
        // it reflects how many rows the CALLER still has past this
        // page. Pre-fix this was computed pre-filter against
        // `limit + 1`, which masked the offset-zero fetch limitation
        // by stamping `has_more = true` even when client-side skip
        // left no real follow-on row. Now `has_more` is the
        // authoritative "should I emit a cursor" signal.
        let has_more = members.len() > limit as usize;
        members.truncate(limit as usize);

        // Encode (last_created_at, last_user_id) of the last emitted
        // user as the next page's cursor. CursorV1 envelope; the
        // realm_name pinning lives inside `filter_hash` now.
        let next_cursor = if has_more {
            members.last().map(|last| {
                Self::encode_cursor(&ListUsersCursor {
                    filter_hash: filter_hash.clone(),
                    last_created_at: last.created_timestamp,
                    last_user_id: last.id,
                })
            })
        } else {
            None
        };

        let items: Vec<IdpUser> = members
            .into_iter()
            .map(Self::user_rep_to_idp_user)
            .collect();

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor,
                prev_cursor: None,
                limit: u64::from(limit),
            },
        ))
    }

    /// Best-effort `POST /admin/realms/{realm}/users/{id}/logout` —
    /// logout-all to invalidate active sessions before the delete. Per
    /// DESIGN "Interactions & Sequences", sessions cleanup is non-fatal: 2xx and 404 both
    /// continue silently; transport / 5xx are logged + ignored so the
    /// delete (the primary effect) still gets a chance to run.
    async fn revoke_sessions(&self, realm_admin: &KcAdminClient, realm_name: &str, user_id: Uuid) {
        let url = format!(
            "{}admin/realms/{}/users/{}/logout",
            self.kc_factory.base_url_str(),
            encode_component(realm_name),
            user_id,
        );
        match realm_admin
            .raw_request(
                "POST",
                &url,
                None,
                "/admin/realms/{realm}/users/{user_id}/logout",
            )
            .await
        {
            Ok((status, _)) => {
                if !(200..=299).contains(&status) && status != 404 {
                    tracing::warn!(
                        target: "keycloak_idp_plugin.user_facade",
                        %status,
                        %user_id,
                        realm = %realm_name,
                        "session-revoke best-effort: KC logout returned non-2xx/404; continuing to DELETE",
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "keycloak_idp_plugin.user_facade",
                    error = %e,
                    %user_id,
                    realm = %realm_name,
                    "session-revoke best-effort: KC logout transport error; continuing to DELETE",
                );
            }
        }
    }

    /// `DELETE /admin/realms/{realm}/users/{user_id}` — the primary
    /// effect of `deprovision_user`. 404 / 410 are folded into success
    /// per the idempotency contract (DESIGN "Interactions & Sequences"). 5xx / transport →
    /// [`PluginError::UserOpUnavailable`].
    async fn delete_user(
        &self,
        realm_admin: &KcAdminClient,
        realm_name: &str,
        user_id: Uuid,
    ) -> Result<(), PluginError> {
        let url = format!(
            "{}admin/realms/{}/users/{}",
            self.kc_factory.base_url_str(),
            encode_component(realm_name),
            user_id,
        );
        let (status, body_text) = realm_admin
            .raw_request(
                "DELETE",
                &url,
                None,
                "/admin/realms/{realm}/users/{user_id}",
            )
            .await
            .map_err(user_translate_kc)?;
        if (200..=299).contains(&status) || status == 404 || status == 410 {
            return Ok(());
        }
        if status == 401 {
            return Err(PluginError::KcRest {
                method: "DELETE",
                path_template: "/admin/realms/{realm}/users/{user_id}".into(),
                status: KcStatusKind::Http(401),
                body_first_2kb: redact_secrets(&truncate_2kb(&body_text)),
            });
        }
        Err(PluginError::UserOpUnavailable {
            detail: format!(
                "kc:DELETE /admin/realms/{{realm}}/users/{{user_id}} -> {status}: {}",
                redact_secrets(&truncate_2kb(&body_text)),
            ),
        })
    }

    /// Read a KC user's stored `tenant_id` attribute — the binding check
    /// primitive for [`Self::deprovision_user_impl`] (`ADR-0006`: `IdP` stores
    /// the user-tenant binding as a tenant identity attribute on the user
    /// record). Returns:
    ///
    /// * `Ok(None)` — KC returned 404 (user absent from this realm), OR the
    ///   user exists but has no `tenant_id` attribute / the value is not a
    ///   parseable `Uuid`. Both collapse to the same "no usable binding"
    ///   outcome — the caller treats this as `mismatch` for safety so that
    ///   data anomalies cannot become silent cross-tenant writes.
    /// * `Ok(Some(uuid))` — user exists with a parseable `tenant_id`.
    /// * `Err(PluginError::KcRest)` with `Http(401)` — surfaced verbatim so
    ///   the enclosing `with_reactive_401` wrapper re-mints the bearer
    ///   and retries the closure once.
    /// * `Err(PluginError::UserOpUnavailable)` — any other non-2xx / non-404
    ///   response or JSON decode failure (transient infra signal — AM may
    ///   retry).
    async fn read_user_owner_tenant(
        &self,
        realm_admin: &KcAdminClient,
        realm_name: &str,
        user_id: Uuid,
    ) -> Result<Option<Uuid>, PluginError> {
        let url = format!(
            "{}admin/realms/{}/users/{}",
            self.kc_factory.base_url_str(),
            encode_component(realm_name),
            user_id,
        );
        let (status, body_text) = realm_admin
            .raw_request("GET", &url, None, "/admin/realms/{realm}/users/{user_id}")
            .await
            .map_err(user_translate_kc)?;
        if status == 404 || status == 410 {
            return Ok(None);
        }
        if status == 401 {
            return Err(PluginError::KcRest {
                method: "GET",
                path_template: "/admin/realms/{realm}/users/{user_id}".into(),
                status: KcStatusKind::Http(401),
                body_first_2kb: redact_secrets(&truncate_2kb(&body_text)),
            });
        }
        if !(200..=299).contains(&status) {
            return Err(PluginError::UserOpUnavailable {
                detail: format!(
                    "kc:GET /admin/realms/{{realm}}/users/{{user_id}} -> {status}: {}",
                    redact_secrets(&truncate_2kb(&body_text)),
                ),
            });
        }
        // Parse the user record loosely — we only need `attributes.tenant_id[0]`,
        // not the full `UserRep`. Tolerating `Value` keeps the helper resilient
        // to future KC representation additions.
        let user: serde_json::Value =
            serde_json::from_str(&body_text).map_err(|e| PluginError::UserOpUnavailable {
                detail: format!(
                    "kc:GET /admin/realms/{{realm}}/users/{{user_id}} json decode: {e}"
                ),
            })?;
        let tid_str = user
            .get("attributes")
            .and_then(|a| a.get("tenant_id"))
            .and_then(|t| t.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str());
        match tid_str {
            Some(s) => Ok(Uuid::parse_str(s).ok()),
            // Attribute absent or malformed — treat as "no binding" so the
            // caller's mismatch check fires. We do NOT return Err here:
            // a data anomaly on the user row should not block the
            // idempotent-absent contract for the deprovision endpoint.
            None => Ok(None),
        }
    }

    /// `GET /admin/realms/{realm}/groups/{tenant_group_id}/members?first={offset}&max={limit}`
    /// — the listing primitive for `list_users`. 2xx → parsed
    /// `Vec<UserRep>`. Any non-2xx → [`PluginError::UserOpUnavailable`]:
    /// a 404 here means the tenant group disappeared (operator /
    /// concurrent delete), which the user-list surface treats as a
    /// provider outage (AM has no way to repair the group from this
    /// path) — same lane as 5xx so AM gets a consistent retry signal.
    async fn fetch_group_members(
        &self,
        realm_admin: &KcAdminClient,
        realm_name: &str,
        tenant_group_id: Uuid,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<UserRep>, PluginError> {
        let url = format!(
            "{}admin/realms/{}/groups/{}/members?first={}&max={}",
            self.kc_factory.base_url_str(),
            encode_component(realm_name),
            tenant_group_id,
            offset,
            limit,
        );
        let (status, body_text) = realm_admin
            .raw_request(
                "GET",
                &url,
                None,
                "/admin/realms/{realm}/groups/{tenant_group_id}/members",
            )
            .await
            .map_err(user_translate_kc)?;
        if status == 401 {
            return Err(PluginError::KcRest {
                method: "GET",
                path_template: "/admin/realms/{realm}/groups/{tenant_group_id}/members".into(),
                status: KcStatusKind::Http(401),
                body_first_2kb: redact_secrets(&truncate_2kb(&body_text)),
            });
        }
        if !(200..=299).contains(&status) {
            return Err(PluginError::UserOpUnavailable {
                detail: format!(
                    "kc:GET /admin/realms/{{realm}}/groups/{{tenant_group_id}}/members -> {status}: {}",
                    redact_secrets(&truncate_2kb(&body_text)),
                ),
            });
        }
        serde_json::from_str::<Vec<UserRep>>(&body_text)
            .map_err(|e| PluginError::UserOpUnavailable {
                detail: format!(
                    "kc:GET /admin/realms/{{realm}}/groups/{{tenant_group_id}}/members json decode: {e}",
                ),
            })
    }

    /// Project a Keycloak [`UserRep`] into the SDK-facing [`IdpUser`].
    /// `firstName + lastName` populates `display_name` when at least one
    /// is present (KC convention; OIDC `name` claim is not surfaced by
    /// the admin-REST shape).
    fn user_rep_to_idp_user(rep: UserRep) -> IdpUser {
        let username = rep.username.unwrap_or_default();
        let mut user = IdpUser::new(rep.id, username);
        if let Some(email) = rep.email {
            user = user.with_email(email);
        }
        let display_name = match (rep.first_name, rep.last_name) {
            (Some(f), Some(l)) if !f.is_empty() && !l.is_empty() => Some(format!("{f} {l}")),
            (Some(f), _) if !f.is_empty() => Some(f),
            (_, Some(l)) if !l.is_empty() => Some(l),
            _ => None,
        };
        if let Some(dn) = display_name {
            user = user.with_display_name(dn);
        }
        user
    }

    /// Extract the canonical `id eq <uuid>` shape from a SDK filter.
    /// Mirrors `IdpListUsersRequest::with_id`. Returns `None` when the
    /// id-eq clause is not present at the top level or under a
    /// supported `and` composition; the rest of the filter (string
    /// fields) is handled by [`Self::build_string_matcher`].
    fn extract_id_filter(filter: Option<&FilterNode<IdpUserFilterField>>) -> Option<Uuid> {
        let node = filter?;
        // Drill through one level of `and` so a caller may combine
        // `id eq <uuid> and username eq '<name>'` — the id-eq half is
        // captured here; the string half is captured by
        // `build_string_matcher` which walks the same tree.
        match node {
            FilterNode::Binary {
                field: IdpUserFilterField::Id,
                op: FilterOp::Eq,
                value: ODataValue::Uuid(uuid),
            } => Some(*uuid),
            FilterNode::Composite {
                op: FilterOp::And,
                children,
            } => children
                .iter()
                .find_map(|child| Self::extract_id_filter(Some(child))),
            _ => None,
        }
    }

    /// Lower the typed `OData` `$filter` into a [`StringMatcher`]
    /// predicate evaluated client-side over the KC-fetched member set.
    ///
    /// Supported shapes (the `/users` list contract):
    ///   * `<string_field> eq '<literal>'` — exact, case-sensitive
    ///   * `<string_field> contains '<literal>'` — substring, case-insensitive
    ///   * `<a> and <b> …`, `<a> or <b> …` — nested freely
    ///   * `id in (<uuid>, …)` — exact UUID set, including under `and` / `or`
    ///   * `id eq <uuid>` — captured by [`Self::extract_id_filter`];
    ///     inside a top-level `and` it lowers to
    ///     [`StringMatcher::Always`] here.
    ///
    /// String fields: `username` / `email` / `first_name` /
    /// `last_name` / `display_name`.
    ///
    /// Unsupported shapes surface as [`PluginError::UserOpUnsupported`]
    /// (HTTP 501): operators other than the shapes above (`ne`, non-ID `in`,
    /// `not`, `startswith`, `endswith`, comparisons), a literal of the wrong
    /// type, or `id eq` nested inside an `or` (the equality half cannot be
    /// hoisted out of a disjunction).
    /// `None` (no `$filter`) lowers to [`StringMatcher::Always`].
    fn build_string_matcher(
        filter: Option<&FilterNode<IdpUserFilterField>>,
    ) -> Result<StringMatcher, PluginError> {
        match filter {
            None => Ok(StringMatcher::Always),
            Some(node) => Self::build_string_matcher_node(node, false),
        }
    }

    fn build_string_matcher_node(
        node: &FilterNode<IdpUserFilterField>,
        in_or: bool,
    ) -> Result<StringMatcher, PluginError> {
        match node {
            // `id eq <uuid>` is captured by `extract_id_filter`. Inside
            // an `and` it is the identity here; inside an `or` it cannot
            // be hoisted out, so reject rather than silently widen.
            FilterNode::Binary {
                field: IdpUserFilterField::Id,
                op: FilterOp::Eq,
                ..
            } => {
                if in_or {
                    Err(PluginError::UserOpUnsupported {
                        detail: "`id eq` inside `or` is not supported on /users filter".into(),
                    })
                } else {
                    Ok(StringMatcher::Always)
                }
            }
            FilterNode::Binary {
                field,
                op: op @ (FilterOp::Eq | FilterOp::Contains),
                value,
            } => {
                let needle = match value {
                    ODataValue::String(s) => s.clone(),
                    other => {
                        return Err(PluginError::UserOpUnsupported {
                            detail: format!(
                                "filter field `{field:?}`: expected string literal, got {other}"
                            ),
                        });
                    }
                };
                let match_op = match op {
                    FilterOp::Contains => StringMatchOp::Contains,
                    _ => StringMatchOp::Eq,
                };
                Ok(StringMatcher::Field {
                    field: *field,
                    op: match_op,
                    needle,
                })
            }
            FilterNode::Binary { op, .. } => Err(PluginError::UserOpUnsupported {
                detail: format!(
                    "operator `{op}` not supported on /users filter; \
                     supported: `eq`, `contains`, `and`, `or`"
                ),
            }),
            FilterNode::Composite {
                op: FilterOp::And,
                children,
            } => {
                let parts = children
                    .iter()
                    .map(|c| Self::build_string_matcher_node(c, in_or))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(StringMatcher::All(parts))
            }
            FilterNode::Composite {
                op: FilterOp::Or,
                children,
            } => {
                let parts = children
                    .iter()
                    .map(|c| Self::build_string_matcher_node(c, true))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(StringMatcher::Any(parts))
            }
            FilterNode::Composite { op, .. } => Err(PluginError::UserOpUnsupported {
                detail: format!(
                    "composite operator `{op}` not supported on /users filter; \
                     supported: `and`, `or`"
                ),
            }),
            FilterNode::InList {
                field: IdpUserFilterField::Id,
                values,
            } => {
                let ids = values
                    .iter()
                    .map(|value| match value {
                        ODataValue::Uuid(id) => Ok(*id),
                        _ => Err(PluginError::UserOpUnsupported {
                            detail: "id-set lookup requires UUID literals".into(),
                        }),
                    })
                    .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
                Ok(StringMatcher::IdSet(ids))
            }
            FilterNode::InList { .. } => Err(PluginError::UserOpUnsupported {
                detail: "operator `in` not supported on /users filter".into(),
            }),
            FilterNode::Not(_) => Err(PluginError::UserOpUnsupported {
                detail: "operator `not` not supported on /users filter".into(),
            }),
        }
    }

    /// Mirror of [`Self::user_rep_to_idp_user`]'s `display_name`
    /// derivation, used for client-side filter matching without
    /// fully projecting the rep first.
    fn user_rep_display_name(rep: &UserRep) -> Option<String> {
        match (&rep.first_name, &rep.last_name) {
            (Some(f), Some(l)) if !f.is_empty() && !l.is_empty() => Some(format!("{f} {l}")),
            (Some(f), _) if !f.is_empty() => Some(f.clone()),
            (_, Some(l)) if !l.is_empty() => Some(l.clone()),
            _ => None,
        }
    }

    /// Short hash of `(tenant_id, realm_name, id_filter,
    /// string_filters)` stuffed into [`CursorV1::f`] so cursor
    /// continuation requests carry their original filter context.
    /// FNV-1a 64-bit rendered as 16 hex chars — adequate for the small
    /// filter space the API exposes and well below
    /// [`IdpUserPagination::MAX_CURSOR_LEN`].
    ///
    /// `realm_name` is folded into the hash so a tenant whose
    /// realm binding changes between pages surfaces as `invalid
    /// cursor` through the same path as a filter / tenant change —
    /// this replaces the pre-CursorV1 explicit `decoded.realm_name
    /// != metadata.realm_name` reject (same effective check, now
    /// bundled in the hash).
    ///
    /// Including the string filter set is load-bearing: a client
    /// that pages through `username eq 'alice'` results and then
    /// changes the filter MUST be rejected rather than silently
    /// jumping between result sets. The [`StringMatcher::feed_hash`]
    /// helper encodes the predicate tree canonically (variant tag +
    /// length-prefixed parts) so a cursor is only valid for an
    /// identical filter.
    fn filter_hash(
        tenant_id: Uuid,
        realm_name: &str,
        user_id_filter: Option<Uuid>,
        matcher: &StringMatcher,
    ) -> String {
        let mut h = FNV1A_BASIS;
        fnv1a_update(&mut h, tenant_id.as_bytes());
        // Length-prefix the realm name so `realm_name="ab" + ...` and
        // `realm_name="a" + "b" + ...` can't collide via concatenation.
        fnv1a_update(&mut h, realm_name.len().to_be_bytes());
        fnv1a_update(&mut h, realm_name.as_bytes());
        // Distinguish "no filter" from "filter = Uuid::nil()" by salting
        // the hash with a 1-byte presence tag.
        match user_id_filter {
            Some(u) => {
                fnv1a_update(&mut h, [1u8]);
                fnv1a_update(&mut h, u.as_bytes());
            }
            None => {
                fnv1a_update(&mut h, [0u8]);
            }
        }
        matcher.feed_hash(&mut h);
        // 16 hex chars (64-bit) is plenty for a cursor fingerprint.
        format!("{h:016x}")
    }

    /// Encode the opaque pagination cursor as
    /// [`CursorV1`]-base64url-JSON. Wire-compatible with AM's
    /// tenant `list_children` cursor (and any other toolkit-odata
    /// consumer) so the `OData` query extractor on the *next* request
    /// parses our cursor through the exact same `CursorV1::decode`
    /// path, with no plugin-specific extractor hook.
    ///
    /// The `k`/`o`/`s` triple pins the plugin's hard-coded sort
    /// order; AM's handler recovers `req.order` from `cursor.s` via
    /// `ODataOrderBy::from_signed_tokens`, so a continuation request
    /// without `$orderby` still reaches the plugin with the right
    /// effective order.
    //
    // `CursorV1::encode` returns `serde_json::Result<String>`;
    // serialisation can only fail on a non-serialisable inner type,
    // and CursorV1's wire shape is all primitives. The `expect`
    // anchors the invariant in the same style as the previous
    // hand-rolled implementation.
    #[expect(
        clippy::expect_used,
        reason = "CursorV1 has no serialiser-defeating shape; the expect anchors that invariant"
    )]
    fn encode_cursor(cursor: &ListUsersCursor) -> String {
        CursorV1 {
            k: vec![
                cursor.last_created_at.to_string(),
                cursor.last_user_id.to_string(),
            ],
            o: CURSOR_ORDER_DIR,
            s: CURSOR_ORDER_TOKENS.to_owned(),
            f: Some(cursor.filter_hash.clone()),
            d: CURSOR_DIRECTION.to_owned(),
        }
        .encode()
        .expect("CursorV1 with primitive fields is always serialisable")
    }

    /// Decode the opaque pagination cursor. Failure modes that all
    /// surface as [`PluginError::UserOpRejected`] ("invalid cursor"):
    ///
    ///   * Malformed base64 / unsupported `v` / shape mismatch from
    ///     [`CursorV1::decode`].
    ///   * `o` or `s` deviates from the plugin's hard-coded
    ///     `(createdTimestamp ASC, id ASC)` sort. A request that
    ///     somehow surfaces a different order-pin almost certainly
    ///     came from a different list endpoint's cursor — reject so
    ///     the caller cannot accidentally walk our pages with
    ///     someone else's tiebreakers.
    ///   * `k` doesn't carry exactly the two tiebreaker strings the
    ///     `(created_at, id)` shape expects, or those strings don't
    ///     parse back to `i64` / `Uuid`.
    ///   * `f` (`filter_hash`) is absent — the cursor was emitted
    ///     under "no filter context", which the plugin always sets,
    ///     so this is structurally invalid.
    fn decode_cursor(raw: &str) -> Result<ListUsersCursor, PluginError> {
        let invalid = || PluginError::UserOpRejected {
            detail: "invalid cursor".into(),
        };
        let cursor = CursorV1::decode(raw).map_err(|_| invalid())?;
        if cursor.o != CURSOR_ORDER_DIR || cursor.s != CURSOR_ORDER_TOKENS {
            return Err(invalid());
        }
        let [created, id] = cursor.k.as_slice() else {
            return Err(invalid());
        };
        let last_created_at: i64 = created.parse().map_err(|_| invalid())?;
        let last_user_id: Uuid = id.parse().map_err(|_| invalid())?;
        let filter_hash = cursor.f.ok_or_else(invalid)?;
        Ok(ListUsersCursor {
            filter_hash,
            last_created_at,
            last_user_id,
        })
    }

    /// Production decode used by [`UserFacade::list_users_impl`]. Tries
    /// the modern [`Self::decode_cursor`] shape first and validates its
    /// `filter_hash` against the current request's modern hash. On
    /// failure, falls back to the pre-CursorV1
    /// [`LegacyListUsersCursor`] shape for the rolling-deploy grace
    /// window: a pre-deploy client whose page 1 lands on the new
    /// build still gets page 2 served correctly (realm + legacy
    /// `filter_hash` are revalidated against the current request).
    ///
    /// Returns the `(last_created_at, last_user_id)` tiebreaker pair
    /// once validation passes. Any failure path (parse, shape, realm
    /// mismatch, filter mismatch) collapses to
    /// `PluginError::UserOpRejected { detail: "invalid cursor" }` so
    /// the caller cannot distinguish between modern-invalid and
    /// legacy-invalid (both are equally "client must re-paginate").
    fn decode_cursor_with_legacy_fallback(
        raw: &str,
        expected_realm: &str,
        expected_tenant_id: Uuid,
        expected_id_filter: Option<Uuid>,
        expected_modern_filter_hash: &str,
    ) -> Result<(i64, Uuid), PluginError> {
        let invalid = || PluginError::UserOpRejected {
            detail: "invalid cursor".into(),
        };

        // --- Modern shape: validated by `decode_cursor`, then compare
        //     filter_hash against the current request's context.
        if let Ok(modern) = Self::decode_cursor(raw) {
            if modern.filter_hash != expected_modern_filter_hash {
                return Err(invalid());
            }
            return Ok((modern.last_created_at, modern.last_user_id));
        }

        // --- Legacy shape: pre-CursorV1 base64url(serde_json).
        //     Revalidate realm + legacy filter context so a query
        //     change mid-walk still rejects, matching the modern
        //     path's safety guarantee.
        let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| invalid())?;
        let legacy: LegacyListUsersCursor =
            serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if legacy.realm_name != expected_realm {
            return Err(invalid());
        }
        if legacy.filter_hash != legacy_filter_hash(expected_tenant_id, expected_id_filter) {
            return Err(invalid());
        }
        let last_created_at = legacy.last_created_at.ok_or_else(invalid)?;
        let last_user_id = legacy.last_user_id.ok_or_else(invalid)?;
        Ok((last_created_at, last_user_id))
    }
}

#[cfg(test)]
#[path = "user_facade_tests.rs"]
mod user_facade_tests;
