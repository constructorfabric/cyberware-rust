//! `Service::list` — the collection read (`GET /credstore/v1/credentials`,
//! ADR-0005/ADR-0004).
//!
//! Child module of [`crate::domain::secret::service`] (see that module's
//! `mod list;` doc comment for why): this file's `impl Service` block
//! reaches `Service`'s private fields and helpers (`repo`, `dir`, `plugins`,
//! `metrics`, `scope_for_timed`, `resolve_stored`, `read_value_for_row`)
//! without exposing any of them beyond the `service` module subtree.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use credstore_sdk::{
    CredStorePluginClientV1, Credential, CredentialListItem, OwnerId, SecretRef, TenantId,
    Validator,
};
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, Page, PageInfo, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{self, actions};
use crate::domain::error::DomainError;
use crate::domain::secret::list_filter::{self, ListDirection, ParsedFilter};
use crate::domain::secret::model::SecretRow;
use crate::domain::secret::reduce::{self, Reduced};
use crate::domain::secret::type_resolver::ResolvedSecretType;

use super::Service;

/// Default page size when the caller sends no `limit`/`$top` (ADR-0005
/// follows the platform's generic pagination default; credstore names no
/// smaller default of its own).
const DEFAULT_LIMIT: u64 = 50;

/// Map a [`toolkit_odata::Error`] (cursor/order/filter-consistency failures
/// the toolkit itself detects) onto the domain error shape the rest of this
/// module's validation already uses, so every rejection — toolkit-detected
/// or credstore-specific — renders through the same `CredentialResource`
/// mapping at the REST boundary.
fn map_odata_err(err: &toolkit_odata::Error) -> DomainError {
    use toolkit_odata::Error;
    let (field, reason): (&'static str, &'static str) = match err {
        Error::OrderMismatch => ("$orderby", "ORDER_MISMATCH"),
        Error::FilterMismatch => ("$filter", "FILTER_MISMATCH"),
        Error::InvalidCursor
        | Error::CursorInvalidBase64
        | Error::CursorInvalidJson
        | Error::CursorInvalidVersion
        | Error::CursorInvalidKeys
        | Error::CursorInvalidFields
        | Error::CursorInvalidDirection => ("cursor", "INVALID_CURSOR"),
        Error::InvalidOrderByField(_) => ("$orderby", "INVALID_ORDERBY_FIELD"),
        Error::InvalidFilter(_) => ("$filter", "INVALID_FILTER"),
        Error::InvalidLimit => ("limit", "INVALID_LIMIT"),
        Error::OrderWithCursor => ("$orderby", "ORDER_WITH_CURSOR"),
        Error::Db(_) | Error::ParsingUnavailable(_) => {
            return DomainError::internal(format!("OData: {err}"));
        }
    };
    DomainError::InvalidRequest {
        field,
        reason,
        detail: err.to_string(),
    }
}

fn invalid_cursor(detail: impl Into<String>) -> DomainError {
    DomainError::InvalidRequest {
        field: "cursor",
        reason: "INVALID_CURSOR",
        detail: detail.into(),
    }
}

/// Build the `Credential` a reduced reference resolves to — the same
/// construction [`Service::resolve_credential`] applies for the point read,
/// so a list item and a point read of the same reference always agree.
fn build_credential(key: &SecretRef, secret_type: String, reduced: &Reduced<'_>) -> Credential {
    let (fallback, version, updated_at, owner_id, validator) = match reduced.own {
        Some(o) => (
            Some(credstore_sdk::Fallback::from(o.fallback)),
            Some(o.version),
            Some(o.updated_at),
            Some(o.owner_id),
            Some(Validator {
                id: o.id,
                version: o.version,
            }),
        ),
        None => (None, None, None, None, None),
    };
    Credential {
        reference: key.clone(),
        secret_type,
        sharing: reduced.effective.sharing,
        fallback,
        status: reduced.own_status(),
        inheritance: reduced.inheritance,
        version,
        updated_at,
        owner_id,
        expires_at: reduced.effective.expires_at,
        validator,
    }
}

impl Service {
    /// The collection read (ADR-0005): one reduced item per reference,
    /// rooted at the caller's tenant and walking upward through its
    /// ancestor chain only. Selecting `secret` in `query`'s `$select`
    /// switches to value mode (ADR-0004, "Bulk secret read").
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidRequest`] for every validation failure
    /// this endpoint defines (see [`crate::domain::secret::list_filter`]):
    /// an unsupported `$filter`/`$orderby`/`$select` field or shape, an
    /// out-of-range `limit`, a malformed or inconsistent cursor, or (value
    /// mode) pagination present, a missing/invalid selector, or a
    /// match-set over the configured cap.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<CredentialListItem>, DomainError> {
        if let Some(fields) = query.selected_fields() {
            list_filter::validate_select(fields)?;
        }
        let parsed_filter = match query.filter() {
            Some(expr) => list_filter::parse_filter(expr)?,
            None => ParsedFilter::default(),
        };

        if list_filter::is_value_mode(query.selected_fields()) {
            self.list_value_mode(ctx, query, &parsed_filter).await
        } else {
            self.list_metadata_mode(ctx, query, &parsed_filter).await
        }
    }

    async fn list_metadata_mode(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
        parsed_filter: &ParsedFilter,
    ) -> Result<Page<CredentialListItem>, DomainError> {
        let limit = match query.limit {
            Some(l) if l > self.list.max_limit => {
                return Err(DomainError::InvalidRequest {
                    field: "limit",
                    reason: "INVALID_LIMIT",
                    detail: format!("limit must be <= {}", self.list.max_limit),
                });
            }
            Some(l) => l,
            None => DEFAULT_LIMIT.min(self.list.max_limit),
        };

        let (direction, cursor_reference) = if let Some(cursor) = &query.cursor {
            let effective_order =
                ODataOrderBy::from_signed_tokens(&cursor.s).map_err(|e| map_odata_err(&e))?;
            let [key] = effective_order.0.as_slice() else {
                return Err(invalid_cursor("cursor does not name a single sort key"));
            };
            if !key.field.eq_ignore_ascii_case("reference") {
                return Err(invalid_cursor("cursor was not minted by this endpoint"));
            }
            toolkit_odata::validate_cursor_against(
                cursor,
                &effective_order,
                query.filter_hash.as_deref(),
            )
            .map_err(|e| map_odata_err(&e))?;
            let [only_key] = cursor.k.as_slice() else {
                return Err(invalid_cursor("cursor does not carry exactly one key"));
            };
            let direction = match cursor.o {
                SortDir::Asc => ListDirection::Asc,
                SortDir::Desc => ListDirection::Desc,
            };
            (direction, Some(only_key.clone()))
        } else {
            (list_filter::validate_metadata_orderby(&query.order)?, None)
        };

        let req = TenantId(ctx.subject_tenant_id());
        let subject = OwnerId(ctx.subject_id());
        let chain = self.dir.ancestor_chain(ctx, req).await?;

        let fetch_limit = limit
            .checked_add(1)
            .ok_or_else(|| DomainError::internal("limit + 1 overflowed u64"))?;
        let mut refs = self
            .repo
            .list_candidate_references(
                req,
                subject,
                &chain,
                parsed_filter.reference_in.as_deref(),
                parsed_filter.type_uuid_in.as_deref(),
                cursor_reference.as_deref(),
                direction.is_desc(),
                fetch_limit,
            )
            .await?;

        let has_more = refs.len() as u64 > limit;
        if has_more {
            refs.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }

        let items = self
            .reduce_and_authorize(
                ctx,
                req,
                subject,
                &chain,
                &refs,
                parsed_filter,
                actions::LIST,
                false,
            )
            .await?;

        let next_cursor = if has_more {
            let last_reference = refs
                .last()
                .cloned()
                .ok_or_else(|| DomainError::internal("has_more true but no references fetched"))?;
            let order_dir = if direction.is_desc() {
                SortDir::Desc
            } else {
                SortDir::Asc
            };
            let order = ODataOrderBy(vec![OrderKey {
                field: "reference".to_owned(),
                dir: order_dir,
            }]);
            let cursor = CursorV1 {
                k: vec![last_reference],
                o: order_dir,
                s: order.to_signed_tokens(),
                f: query.filter_hash.clone(),
                d: "fwd".to_owned(),
            };
            Some(
                cursor
                    .encode()
                    .map_err(|e| DomainError::internal(format!("cursor encode failed: {e}")))?,
            )
        } else {
            None
        };

        Ok(Page {
            items,
            page_info: PageInfo {
                next_cursor,
                // Backward pagination is out of scope for this endpoint
                // (ADR-0005 describes only a forward walk); always `None`.
                prev_cursor: None,
                limit,
            },
        })
    }

    async fn list_value_mode(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
        parsed_filter: &ParsedFilter,
    ) -> Result<Page<CredentialListItem>, DomainError> {
        if query.limit.is_some() || query.cursor.is_some() {
            return Err(DomainError::InvalidRequest {
                field: if query.limit.is_some() {
                    "limit"
                } else {
                    "cursor"
                },
                reason: list_filter::reasons::VALUE_MODE_NO_PAGINATION,
                detail: "value mode does not paginate; remove limit/cursor".to_owned(),
            });
        }
        if !query.order.is_empty() {
            return Err(DomainError::InvalidRequest {
                field: "$orderby",
                reason: list_filter::reasons::VALUE_MODE_NO_ORDER,
                detail: "value mode has no $orderby".to_owned(),
            });
        }
        parsed_filter.require_value_mode_selector()?;

        let req = TenantId(ctx.subject_tenant_id());
        let subject = OwnerId(ctx.subject_id());
        let chain = self.dir.ancestor_chain(ctx, req).await?;

        let cap = self.list.value_mode_cap;
        let cap_plus_one = cap
            .checked_add(1)
            .ok_or_else(|| DomainError::internal("value_mode_cap + 1 overflowed u64"))?;
        let refs = self
            .repo
            .list_candidate_references(
                req,
                subject,
                &chain,
                parsed_filter.reference_in.as_deref(),
                parsed_filter.type_uuid_in.as_deref(),
                None,
                false,
                cap_plus_one,
            )
            .await?;

        if refs.len() as u64 > cap {
            return Err(DomainError::InvalidRequest {
                field: "$filter",
                reason: list_filter::reasons::TOO_MANY_MATCHES,
                detail: format!("selector matches more than {cap} references; narrow it"),
            });
        }

        let items = self
            .reduce_and_authorize(
                ctx,
                req,
                subject,
                &chain,
                &refs,
                parsed_filter,
                actions::READ_SECRET,
                true,
            )
            .await?;

        Ok(Page {
            items,
            page_info: PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: cap,
            },
        })
    }

    /// Shared tail of both modes (ADR-0005 steps 6-9): authorize each
    /// distinct type found among `references`, fetch those references'
    /// rows whole (unclamped by type), reduce each to one item, drop what
    /// the caller may not see, apply the in-memory filters, and — in value
    /// mode — read each winner's value.
    #[allow(
        clippy::too_many_arguments,
        reason = "every input the shared reduction+authorization tail needs; splitting it into \
                  a struct would only move the same eight names one level down"
    )]
    async fn reduce_and_authorize(
        &self,
        ctx: &SecurityContext,
        req: TenantId,
        subject: OwnerId,
        chain: &[Uuid],
        references: &[String],
        parsed_filter: &ParsedFilter,
        action: &str,
        value_mode: bool,
    ) -> Result<Vec<CredentialListItem>, DomainError> {
        if references.is_empty() {
            return Ok(Vec::new());
        }

        // Authorization: one PDP evaluation per distinct type found under
        // the SAME clamp step 1 applied (not a scan of every row of
        // `references` regardless of type) — see
        // `list_candidate_types`'s doc comment for why this specific
        // clamping is what lets an override-type-consistency violation be
        // told apart from an ordinary denial below.
        let type_uuids = self
            .repo
            .list_candidate_types(
                req,
                subject,
                chain,
                references,
                parsed_filter.type_uuid_in.as_deref(),
            )
            .await?;
        let found_types: HashSet<Uuid> = type_uuids.iter().copied().collect();

        let mut allowed_types: HashMap<Uuid, ResolvedSecretType> = HashMap::new();
        for type_uuid in type_uuids {
            let resolved = self.resolve_stored(type_uuid).await?;
            let scope = match self
                .scope_for_timed(
                    ctx,
                    &authz::credential_type_resource(&resolved.gts_id),
                    action,
                )
                .await
            {
                Ok(scope) => scope,
                Err(DomainError::AccessDenied { .. }) => continue,
                Err(e) => return Err(e),
            };
            if self.repo.scope_includes_tenant(&scope, req.0).await? {
                allowed_types.insert(type_uuid, resolved);
            } else {
                self.metrics.cross_tenant_denied();
            }
        }
        // A caller the gate refuses gets an empty page, never a refusal
        // (ADR-0005 §"How authorization applies to a collection", step 4):
        // there is simply no admitted type left to build items from.
        if allowed_types.is_empty() {
            return Ok(Vec::new());
        }

        let rows = self
            .repo
            .list_candidates_for_references(req, subject, chain, references)
            .await?;
        let mut by_reference: HashMap<String, Vec<SecretRow>> = HashMap::new();
        for row in rows {
            by_reference
                .entry(row.reference.clone())
                .or_default()
                .push(row);
        }

        let plugin: Option<Arc<dyn CredStorePluginClientV1>> = if value_mode {
            Some(self.plugins.resolve().await?)
        } else {
            None
        };

        let mut items = Vec::with_capacity(references.len());
        for reference in references {
            let Some(group) = by_reference.get(reference) else {
                // Step 1 selected this reference because a row matching its
                // predicate existed; step 2 uses the same predicate over
                // the same reference, so this is structurally unreachable.
                continue;
            };
            let Some(reduced) = reduce::reduce_reference(group, req, subject, chain) else {
                continue;
            };

            let effective_type = reduced.effective.secret_type_uuid;
            let Some(resolved) = allowed_types.get(&effective_type) else {
                if !found_types.contains(&effective_type) {
                    // The winner's type was never even among the types step
                    // 1's clamp selected for — the override-type-consistency
                    // invariant was violated for this reference (ADR-0005
                    // §"Filter in SQL first…"); a missing catalogue entry
                    // and an operational signal, not a false one.
                    self.metrics.list_type_invariant_violation();
                }
                continue;
            };

            if !parsed_filter.matches_post_reduction(
                reduced.effective.sharing,
                reduced.own.map(|o| o.fallback),
                reduced.effective.expires_at,
            ) {
                continue;
            }

            let key = SecretRef::new(reference.clone()).map_err(|e| {
                DomainError::internal(format!("stored reference failed SecretRef validation: {e}"))
            })?;
            let credential = build_credential(&key, resolved.gts_id.clone(), &reduced);

            if !value_mode {
                items.push(CredentialListItem {
                    credential,
                    secret: None,
                });
                continue;
            }

            let Some(winner) = reduced.winner else {
                continue;
            };
            if winner.value_id.is_none() {
                continue;
            }
            // Set whenever `value_mode` is true, which is the only path
            // that reaches here (see the early `continue` above).
            let Some(plugin) = plugin.as_ref() else {
                return Err(DomainError::internal(
                    "value mode reached the read step without a resolved plugin",
                ));
            };
            let secret = self
                .read_value_for_row(
                    plugin,
                    ctx,
                    req,
                    subject,
                    &key,
                    chain,
                    winner,
                    &resolved.gts_id,
                )
                .await?;
            if let Some(secret) = secret {
                items.push(CredentialListItem {
                    credential,
                    secret: Some(secret.value),
                });
            }
            // A refused/missing/fingerprint-mismatched value is omitted,
            // not reported (ADR-0004 "Bulk secret read: the collection in
            // value mode") — `read_value_for_row` already recorded the
            // relevant metric.
        }

        Ok(items)
    }
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
