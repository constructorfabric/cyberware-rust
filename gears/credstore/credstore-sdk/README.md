# `CredStore` SDK

SDK crate for the `CredStore` gear, providing public API contracts for credential storage in Gears.

## Overview

This crate defines the transport-agnostic interface for the `CredStore` gear:

- **`CredStoreClientV1`** — consumer-facing trait (`get`/`put`/`create`/`delete`);
  `get` returns the value plus metadata (`owner_tenant_id`, `sharing`,
  `is_inherited`, `id`, `version`, `secret_type`, `expires_at`)
- **`CredStorePluginClientV1`** — backend trait: a pure per-tenant value store
  (`get`/`put`/`delete` keyed by `tenant_id` + `key` + optional `owner_id`); it
  holds no sharing/hierarchy/policy — that lives in the gear. Planned,
  ADR-0006, not yet implemented: the key becomes `tenant_id`/`value_id` — no
  `key`, no `owner_id` — since backend entries become immutable, addressed
  only by version, and unique store-wide; the plugin learns nothing about
  references, owners, or sharing
- **`SecretRef`** / **`SecretValue`** / **`SharingMode`** / **`GetSecretResponse`** — Domain models
- **`CredStoreError`** — Error types for all operations
- **`CredStorePluginSpecV1`** — GTS schema for plugin registration
- Planned (ADR-0004), not yet implemented — `CredStoreClientV1` reshaped
  around the credential record and its secret value: `get`, `get_secret`,
  `put`, `patch`, `list`, `delete`.
  - `get` — point read of one credential record, without its value; also the
    source of the `ETag` a value-blind writer needs
  - `get_secret` — hierarchical read of the value, at its own address
  - `put` — precondition-guarded create-or-replace of the record together
    with its value, in one call; the create-only precondition is how a
    credential is created
  - `patch` — precondition-guarded partial update following RFC 7396
    merge-patch semantics: present fields replace, absent fields are
    untouched; metadata edit, value rotate, or value remove (a `null` value)
    all go through it; never creates
  - `list` — takes an OData query (`filter`, `select`, `orderby`, `limit`,
    `cursor`) over credential records; an item's `secret` field is present
    only when `select` names it. Selecting `secret` switches the call into
    **value mode**, matching the REST contract one-for-one: `limit` and
    `cursor` are rejected, results are capped and non-paginated, and only
    `reference in (...)` or `type eq`/`in` may filter — no prefix or ordered
    operator over `reference` — see ADR-0004 for why that one is withheld
    rather than pending. Without `secret` selected, `list` behaves exactly as
    the plain listing: paginated, never carrying values regardless of the
    caller's grants
  - `delete` — precondition-guarded delete of the record and its value

  **`get` changes meaning**: it returns a `Credential`, which has no `value`
  field, so every existing caller fails to compile rather than silently
  reading metadata; value readers move to `get_secret`. **`create` is
  removed**: `put` under the create-only precondition is create, and it
  always carries a value — creating with `put` (record and value together,
  guarded by the create-only precondition), then editing or rotating with
  `patch`; a `patch` `value` of `null` removes the value.

## Usage

A `ToolKit` consumer normally obtains `CredStoreClientV1` from `ClientHub`. The SDK
itself is transport-independent, so the example accepts the resolved client directly:

```no_run
use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use toolkit_security::SecurityContext;

async fn secret_length(
    credstore: &dyn CredStoreClientV1,
    security: &SecurityContext,
) -> Result<Option<usize>, CredStoreError> {
    let key = SecretRef::new("my-api-key")?;
    let response = credstore.get(security, &key).await?;

    Ok(response.map(|secret| secret.value.as_bytes().len()))
}
```

A missing or out-of-scope secret is expressed as `Ok(None)`, preventing existence
leaks. An explicit denial of the read action is returned as `CredStoreError::AccessDenied`.

## License

Apache-2.0
