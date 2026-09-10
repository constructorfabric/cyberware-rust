# `CredStore` SDK

SDK crate for the `CredStore` gear, providing public API contracts for credential storage in Gears.

## Overview

This crate defines the transport-agnostic interface for the `CredStore` gear:

- **`CredStoreClientV1`** — consumer-facing trait (`get`/`put`/`create`/`delete`);
  `get` returns the value plus metadata (`owner_tenant_id`, `sharing`,
  `is_inherited`, `id`, `version`, `secret_type`, `expires_at`)
- **`CredStorePluginClientV1`** — backend trait: a pure per-tenant value store
  (`get`/`put`/`delete` keyed by `tenant_id` + `key` + optional `owner_id`); it
  holds no sharing/hierarchy/policy — that lives in the gear
- **`SecretRef`** / **`SecretValue`** / **`SharingMode`** / **`GetSecretResponse`** — Domain models
- **`CredStoreError`** — Error types for all operations
- **`CredStorePluginSpecV1`** — GTS schema for plugin registration
- Planned (ADR-0004), not yet implemented — `CredStoreClientV1` reshaped
  around the credential record and its secret value, with one noun per
  resource: `get`, `list`, `put`, `delete` address the record; `get_secret`,
  `put_secret`, `read_secrets` address the value.
  - `get` — point read of one credential record, without its value; also the
    source of the `ETag` a value-blind writer needs
  - `list` — listing of credential records; never carries values, regardless
    of the caller's grants
  - `get_secret` / `put_secret` — read, set or rotate the value at its own
    address, under the record's validator
  - `read_secrets` — bulk read of secret values for a bounded selection,
    capped and non-paginated. Exactly one of two selectors per call, matching
    the REST contract one-for-one: an explicit list of references, or a filter
    over allowlisted metadata fields using membership operators only
    (`eq` / `in`). There is no third shape, and no prefix or ordered operator
    over `reference` — see ADR-0004 for why that one is withheld rather than
    pending

  **`get` and `put` change meaning**: `get` returns a `Credential`, which has
  no `value` field, so every existing caller fails to compile rather than
  silently reading metadata; value readers move to `get_secret`. **`create`
  is removed**: it takes a value, and creating a record with its value in one
  call is exactly what the split removes — a caller declares the record with
  `put`, then sets the value with `put_secret`.

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
