//! REST handlers for the credstore module (ADR-0004: the credential surface).

use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use credstore_sdk::{
    CredentialPatch, CredentialWrite, Fallback, GtsId, PatchField, SecretRef, SecretValue,
    SharingMode,
};
use toolkit::api::canonical_prelude::*;
use toolkit::api::page_to_projected_json;
use toolkit_security::SecurityContext;

use super::dto::{
    CredentialDto, CredentialListItemDto, CredentialPatchDto, PutCredentialRequestDto, SecretDto,
    weak_etag,
};
use crate::domain::error::DomainError;
use crate::domain::secret::model::{
    PutPrecondition as DomainPutPrecondition, WritePrecondition as DomainWritePrecondition,
};
use crate::domain::secret::service::Service;
use crate::domain::secret::typing::reasons;

/// Concrete service alias for the handlers.
pub(crate) type ConcreteService = Service;

/// `Content-Type` a `PATCH` body must carry (RFC 7396 JSON Merge Patch).
const MERGE_PATCH_CONTENT_TYPE: &str = "application/merge-patch+json";

/// Parse the mandatory `If-Match` precondition (RFC 7232 §3.1), used by
/// `PATCH` and `DELETE`. A missing header is a typed 400
/// (`IF_MATCH_REQUIRED`).
///
/// `If-Match` is `"*" / 1#entity-tag`: it may span multiple header lines and
/// each line may carry a comma-separated list, and the list matches if **any**
/// validator matches. We accept `*` (target must exist) or one-or-more strong
/// `"<id>.<version>"` validators; a single one yields
/// [`DomainWritePrecondition::Version`], several yield
/// [`DomainWritePrecondition::AnyVersion`]. Weak validators (`W/"…"`) and any
/// other shape are a typed 400.
fn parse_if_match(headers: &axum::http::HeaderMap) -> Result<DomainWritePrecondition, DomainError> {
    let mut lines = headers
        .get_all(axum::http::header::IF_MATCH)
        .iter()
        .peekable();
    if lines.peek().is_none() {
        return Err(DomainError::PreconditionRequired {
            detail: "If-Match is required: send the current ETag from GET (or `*` for an \
                     explicit unconditional overwrite)"
                .to_owned(),
        });
    }
    let malformed = || DomainError::InvalidPrecondition {
        detail: "If-Match must be `*` or quoted `<id>.<version>` ETag(s)".to_owned(),
    };
    let mut validators: Vec<(uuid::Uuid, i64)> = Vec::new();
    for raw in lines {
        let line = raw.to_str().map_err(|_| DomainError::InvalidPrecondition {
            detail: "If-Match header is not valid ASCII".to_owned(),
        })?;
        for tag in line.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            // `*` matches any current representation → an existence check; it
            // subsumes any other member, so return immediately.
            if tag == "*" {
                return Ok(DomainWritePrecondition::Exists);
            }
            // Strong validator only: `"<id>.<version>"` (a UUID contains no
            // `.`, so the split is unambiguous).
            let parsed = tag
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .and_then(|inner| inner.split_once('.'))
                .and_then(|(id, version)| {
                    Some((
                        uuid::Uuid::parse_str(id).ok()?,
                        version.parse::<i64>().ok()?,
                    ))
                })
                .ok_or_else(malformed)?;
            validators.push(parsed);
        }
    }
    match validators.as_slice() {
        [] => Err(malformed()),
        [(id, version)] => Ok(DomainWritePrecondition::Version {
            id: *id,
            version: *version,
        }),
        _ => Ok(DomainWritePrecondition::AnyVersion(validators)),
    }
}

/// Parse `PUT`'s precondition (ADR-0004, "Two write verbs on one resource"):
/// exactly one of `If-None-Match: *` (create-only) or `If-Match` (guarded or
/// unconditional replace, parsed like [`parse_if_match`]). Neither, or both,
/// is a typed 400 (`PRECONDITION_REQUIRED`); an `If-None-Match` naming
/// anything other than `*` is a typed 400 (`INVALID_IF_MATCH`).
fn parse_put_precondition(
    headers: &axum::http::HeaderMap,
) -> Result<DomainPutPrecondition, DomainError> {
    let has_if_none_match = headers.contains_key(axum::http::header::IF_NONE_MATCH);
    let has_if_match = headers.contains_key(axum::http::header::IF_MATCH);
    match (has_if_none_match, has_if_match) {
        (true, true) => Err(DomainError::InvalidRequest {
            field: "If-Match",
            reason: reasons::PRECONDITION_REQUIRED,
            detail: "send exactly one of If-None-Match or If-Match, not both".to_owned(),
        }),
        (false, false) => Err(DomainError::InvalidRequest {
            field: "If-Match",
            reason: reasons::PRECONDITION_REQUIRED,
            detail: "PUT requires `If-None-Match: *` (create) or `If-Match` (replace)".to_owned(),
        }),
        (true, false) => {
            let value = headers
                .get(axum::http::header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| DomainError::InvalidPrecondition {
                    detail: "If-None-Match must be `*`".to_owned(),
                })?;
            if value.trim() == "*" {
                Ok(DomainPutPrecondition::CreateOnly)
            } else {
                Err(DomainError::InvalidPrecondition {
                    detail: "If-None-Match must be `*` (create-only); a specific ETag is not \
                             meaningful here"
                        .to_owned(),
                })
            }
        }
        (false, true) => Ok(match parse_if_match(headers)? {
            DomainWritePrecondition::Exists => DomainPutPrecondition::Exists,
            DomainWritePrecondition::Version { id, version } => {
                DomainPutPrecondition::Version { id, version }
            }
            DomainWritePrecondition::AnyVersion(v) => DomainPutPrecondition::AnyVersion(v),
        }),
    }
}

/// Parse an RFC 3339 timestamp from a REST field, mapping a malformed value
/// to a typed 400.
fn parse_rfc3339(field: &'static str, raw: &str) -> Result<time::OffsetDateTime, DomainError> {
    time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).map_err(|_| {
        DomainError::InvalidRequest {
            field,
            reason: "INVALID_EXPIRES_AT",
            detail: format!("{field} must be an RFC 3339 timestamp"),
        }
    })
}

/// Parse a full GTS type id from a REST field, mapping a malformed value to
/// a typed 400. Whether a *well-formed* custom type actually exists is
/// decided by the service against the types-registry.
fn parse_gts_type(field: &'static str, raw: &str) -> Result<GtsId, DomainError> {
    GtsId::try_new(raw).map_err(|_| DomainError::TypeViolation {
        field,
        reason: reasons::UNKNOWN_SECRET_TYPE,
        detail: format!("{field} must be a full GTS type id: {raw}"),
    })
}

/// `GET /credstore/v1/credentials` (ADR-0005/ADR-0004): the collection read.
///
/// Metadata-mode responses carry every reduced item as the same shape
/// `GET .../{ref}` returns (`secret` absent); selecting `secret` in
/// `$select` switches to value mode (ADR-0004, "Bulk secret read"), whose
/// items additionally carry the decrypted value — audited per item exactly
/// like `GET .../{ref}/secret`, since both paths share
/// `Service::read_value_for_row`'s retry/fence-verification/metrics.
///
/// # Errors
///
/// Returns a canonical `Problem` envelope on an unsupported `$filter`/
/// `$orderby`/`$select` field or shape (400), an out-of-range `limit` or a
/// malformed/inconsistent cursor (400), or — in value mode — pagination
/// present, an invalid selector, or a match-set over the configured cap
/// (400).
pub async fn list_credentials(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    OData(query): OData,
) -> ApiResult<impl IntoResponse> {
    let page = svc.list(&ctx, &query).await?;
    let mut items = Vec::with_capacity(page.items.len());
    for item in &page.items {
        items.push(CredentialListItemDto::try_from_list_item(item)?);
    }
    let dto_page = toolkit_odata::Page {
        items,
        page_info: page.page_info,
    };
    let body = match query.selected_fields() {
        Some(fields) => Json(page_to_projected_json(&dto_page, Some(fields))).into_response(),
        None => Json(dto_page).into_response(),
    };
    Ok((
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        body,
    )
        .into_response())
}

/// `GET /credstore/v1/credentials/{ref}` (ADR-0004).
///
/// # Errors
///
/// Returns a canonical `Problem` envelope on invalid reference (400), access denied (403),
/// or not found (404).
pub async fn get_credential(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path(reference): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let key = SecretRef::new(reference).map_err(|e| {
        CanonicalError::from(DomainError::InvalidSecretRef {
            detail: e.to_string(),
        })
    })?;
    match svc.resolve_credential(&ctx, &key).await? {
        Some((credential, weak_source)) => {
            let dto = CredentialDto::try_from_credential(&credential)?;
            let etag = match (&credential.validator, weak_source) {
                (Some(v), _) => format!("\"{}.{}\"", v.id, v.version),
                (None, Some((winner_id, winner_version))) => weak_etag(
                    ctx.subject_tenant_id(),
                    key.as_ref(),
                    winner_id,
                    winner_version,
                ),
                (None, None) => {
                    // Structurally unreachable: `resolve_credential` only
                    // returns `Some` with no validator when a winner exists.
                    return Err(CanonicalError::from(DomainError::internal(
                        "credential resolved with neither a strong nor a weak validator source",
                    )));
                }
            };
            Ok((
                StatusCode::OK,
                [
                    (axum::http::header::ETAG, etag),
                    (axum::http::header::CACHE_CONTROL, "no-store".to_owned()),
                ],
                Json(dto),
            )
                .into_response())
        }
        None => Err(CanonicalError::from(DomainError::NotFound)),
    }
}

/// `GET /credstore/v1/credentials/{ref}/secret` (ADR-0004).
///
/// # Errors
///
/// Returns a canonical `Problem` envelope on invalid reference (400), access denied (403),
/// or not found (404).
pub async fn get_secret(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path(reference): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let key = SecretRef::new(reference).map_err(|e| {
        CanonicalError::from(DomainError::InvalidSecretRef {
            detail: e.to_string(),
        })
    })?;
    match svc.get_secret(&ctx, &key).await? {
        Some(ref secret) => {
            let dto = SecretDto::try_from_secret(secret)?;
            let etag = format!("\"{}.{}\"", secret.validator.id, secret.validator.version);
            Ok((
                StatusCode::OK,
                [
                    (axum::http::header::ETAG, etag),
                    (axum::http::header::CACHE_CONTROL, "no-store".to_owned()),
                ],
                Json(dto),
            )
                .into_response())
        }
        None => Err(CanonicalError::from(DomainError::NotFound)),
    }
}

/// `PUT /credstore/v1/credentials/{ref}` (ADR-0004) — create or replace the
/// whole credential.
///
/// # Errors
///
/// Returns a canonical `Problem` envelope on invalid reference / malformed or
/// conflicting preconditions (400), access denied (403), a trait or type
/// violation (400/409), or a failed precondition (409).
pub async fn put_credential(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path(reference): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<PutCredentialRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let precondition = parse_put_precondition(&headers)?;
    let key = SecretRef::new(reference).map_err(|e| {
        CanonicalError::from(DomainError::InvalidSecretRef {
            detail: e.to_string(),
        })
    })?;
    let Some(raw_value) = body.value else {
        return Err(CanonicalError::from(DomainError::InvalidRequest {
            field: "value",
            reason: reasons::VALUE_REQUIRED,
            detail: "value is required".to_owned(),
        }));
    };
    let secret_type = body
        .secret_type
        .as_deref()
        .map(|s| parse_gts_type("type", s))
        .transpose()?;
    let expires_at = body
        .expires_at
        .as_deref()
        .map(|s| parse_rfc3339("expires_at", s))
        .transpose()?;

    let write = CredentialWrite {
        secret_type,
        sharing: body.sharing.into(),
        fallback: Fallback::from(body.fallback),
        expires_at,
        value: SecretValue::from(raw_value),
    };
    let outcome = svc.put(&ctx, &key, write, precondition).await?;
    let etag = format!("\"{}.{}\"", outcome.validator.id, outcome.validator.version);
    if outcome.created {
        let location = format!("/credstore/v1/credentials/{}", key.as_ref());
        Ok((
            StatusCode::CREATED,
            [
                (axum::http::header::LOCATION, location),
                (axum::http::header::ETAG, etag),
            ],
        )
            .into_response())
    } else {
        Ok((StatusCode::NO_CONTENT, [(axum::http::header::ETAG, etag)]).into_response())
    }
}

/// `PATCH /credstore/v1/credentials/{ref}` (ADR-0004, RFC 7396 JSON Merge
/// Patch).
///
/// Requires `Content-Type: application/merge-patch+json` (415 otherwise).
/// This is checked here, in the handler, rather than through the standard
/// `Json<T>` extractor: the toolkit's `Json<T>` (like `axum::Json<T>`)
/// requires the body's content type to be exactly `application/json` and
/// answers 415 for anything else — the opposite of what RFC 7396 requires —
/// so the route is registered for `OpenAPI` purposes with
/// `.json_request::<CredentialPatchDto>(...)` (its `content_type` metadata
/// therefore reads `application/json`, a stated documentation deviation),
/// while the handler takes the raw body and checks the header itself.
///
/// # Errors
///
/// Returns a canonical `Problem` envelope on a missing/wrong `Content-Type`
/// (415), invalid reference / malformed body / `If-None-Match` present (400),
/// access denied (403), not found (404), an empty patch or a `null` on a
/// non-nullable field (400), a type violation (400/409), or a failed
/// precondition (409).
pub async fn patch_credential(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path(reference): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_owned());
    if content_type.as_deref() != Some(MERGE_PATCH_CONTENT_TYPE) {
        // A real 415: not a canonical category on its own, so this builds
        // the error directly (rather than through `DomainError`) to attach
        // the HTTP-status override — see `crate::infra::sdk_error_mapping`
        // for the `CredentialResource` marker this mirrors.
        return Err(
            crate::infra::sdk_error_mapping::CredentialResource::invalid_argument()
                .with_field_violation(
                    "Content-Type",
                    format!(
                        "PATCH requires Content-Type: {MERGE_PATCH_CONTENT_TYPE}; got {}",
                        content_type.as_deref().unwrap_or("<none>")
                    ),
                    "UNSUPPORTED_MEDIA_TYPE",
                )
                .with_override(toolkit_canonical_errors::transport::Http::status_code(415))
                .create(),
        );
    }
    if headers.contains_key(axum::http::header::IF_NONE_MATCH) {
        return Err(CanonicalError::from(DomainError::InvalidRequest {
            field: "If-None-Match",
            reason: reasons::PRECONDITION_REQUIRED,
            detail: "If-None-Match is not meaningful on PATCH; use If-Match".to_owned(),
        }));
    }
    let precondition = parse_if_match(&headers)?;
    let key = SecretRef::new(reference).map_err(|e| {
        CanonicalError::from(DomainError::InvalidSecretRef {
            detail: e.to_string(),
        })
    })?;
    let dto: CredentialPatchDto = serde_json::from_slice(&body).map_err(|e| {
        CanonicalError::from(DomainError::InvalidRequest {
            field: "body",
            reason: "INVALID_MERGE_PATCH_BODY",
            detail: format!("the request body is not a readable JSON object: {e}"),
        })
    })?;

    let secret_type = match dto.secret_type {
        None => None,
        Some(None) => {
            return Err(CanonicalError::from(DomainError::InvalidRequest {
                field: "type",
                reason: reasons::NULL_NOT_ALLOWED,
                detail: "type cannot be cleared to null; omit it to leave the type unchanged"
                    .to_owned(),
            }));
        }
        Some(Some(raw)) => Some(parse_gts_type("type", &raw)?),
    };
    let sharing = match dto.sharing {
        None => None,
        Some(None) => {
            return Err(CanonicalError::from(DomainError::InvalidRequest {
                field: "sharing",
                reason: reasons::NULL_NOT_ALLOWED,
                detail: "sharing cannot be cleared to null".to_owned(),
            }));
        }
        Some(Some(s)) => Some(SharingMode::from(s)),
    };
    let fallback = match dto.fallback {
        None => None,
        Some(None) => {
            return Err(CanonicalError::from(DomainError::InvalidRequest {
                field: "fallback",
                reason: reasons::NULL_NOT_ALLOWED,
                detail: "fallback cannot be cleared to null; send \"inherit\" or \"none\""
                    .to_owned(),
            }));
        }
        Some(Some(f)) => Some(Fallback::from(f)),
    };
    let expires_at = match dto.expires_at {
        None => PatchField::Absent,
        Some(None) => PatchField::Null,
        Some(Some(raw)) => PatchField::Set(parse_rfc3339("expires_at", &raw)?),
    };
    let value = match dto.value {
        None => PatchField::Absent,
        Some(None) => PatchField::Null,
        Some(Some(raw)) => PatchField::Set(SecretValue::from(raw)),
    };

    let patch = CredentialPatch {
        secret_type,
        sharing,
        fallback,
        expires_at,
        value,
    };
    let validator = svc.patch(&ctx, &key, patch, precondition).await?;
    let etag = format!("\"{}.{}\"", validator.id, validator.version);
    Ok((StatusCode::NO_CONTENT, [(axum::http::header::ETAG, etag)]).into_response())
}

/// `DELETE /credstore/v1/credentials/{ref}` (ADR-0004).
///
/// `If-Match` is **mandatory** (a version validator, or `*` for an explicit
/// delete-whatever-is-there); a stale version yields a canonical `Aborted`
/// (409, `OPTIMISTIC_LOCK_FAILURE`).
///
/// # Errors
///
/// Returns a canonical `Problem` envelope on invalid reference / malformed or
/// missing `If-Match` (400), access denied (403), not found (404), version
/// precondition failure (409), or service unavailable (503).
pub async fn delete_credential(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path(reference): Path<String>,
    headers: axum::http::HeaderMap,
) -> ApiResult<impl IntoResponse> {
    let precondition = parse_if_match(&headers)?;
    let key = SecretRef::new(reference).map_err(|e| {
        CanonicalError::from(DomainError::InvalidSecretRef {
            detail: e.to_string(),
        })
    })?;
    svc.delete(&ctx, &key, precondition).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
