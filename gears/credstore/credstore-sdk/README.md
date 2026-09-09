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
- Planned (ADR-0004), not yet implemented — additional `CredStoreClientV1`
  contracts for the upcoming credential/secret split:
  - `metadata` — point read of a credential record's metadata only, without
    its value; also the source of the `ETag` a value-blind writer needs
  - `list_metadata` — listing of credential records; never carries values,
    regardless of the caller's grants
  - `read_secrets` — bulk read of secret values for a bounded selection,
    capped and non-paginated. Exactly one of two selectors per call, matching
    the REST contract one-for-one: an explicit list of references, or a filter
    over allowlisted metadata fields using membership operators only
    (`eq` / `in`). There is no third shape, and no prefix or ordered operator
    over `reference` — see ADR-0004 for why that one is withheld rather than
    pending

  These methods will ship with default "unsupported" implementations, so
  existing trait implementors and test doubles keep compiling unchanged.

  `get`, `put` and `delete` keep their names and are re-pointed at the value
  sub-resource and the record. **`create` does not survive as it is:** it
  takes a value, and creating a record with its value in one call is exactly
  what the split removes. It becomes either a convenience wrapper that issues
  both writes and documents its own non-atomicity, or it is dropped in favour
  of a record write followed by a value write — the choice is open, and
  whichever way it goes, a caller that used `create` changes shape.

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
