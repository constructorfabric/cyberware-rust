# `CredStore` SDK

SDK crate for the `CredStore` gear, providing public API contracts for credential storage in Gears.

## Overview

This crate defines the transport-agnostic interface for the `CredStore` gear
(ADR-0004: the credential record and its secret value are separate
resources):

- **`CredStoreClientV1`** — consumer-facing trait, seven methods:
  - `get` — point read of one credential record (`Credential`), without its
    value; also the source of the `ETag` a value-blind writer needs
  - `get_secret` — hierarchical read of the value (`Secret`), at its own
    address; a winning record with no value (`declared`/`suppressed`) is the
    canonical miss
  - `put` — precondition-guarded create-or-replace of the whole credential —
    record and value together, in one call; `PutPrecondition::CreateOnly` is
    how a credential is created (there is no separate `create` method)
  - `patch` — precondition-guarded partial update following RFC 7396
    merge-patch semantics: present fields replace, absent fields are
    untouched; metadata edit, value rotate, or value remove (a `null` value)
    all go through it; never creates
  - `delete` — precondition-guarded delete of the record and its value
  - `list` — the upward-rooted collection read (ADR-0005): one reduced
    `CredentialListItem` per reference, keyset-paginated by `reference`
    (`toolkit_odata::CursorV1`); selecting `secret` in the `OData` `$select`
    switches the request to bulk **value mode** (ADR-0004, "Bulk secret read:
    the collection in value mode") — no pagination, a configured cap, and
    each item's `secret` field populated for what the caller may read
- **`CredStoreMaintenanceV1`** — in-process maintenance entry point
  (ADR-0006): `run_gc` is the periodic garbage-collection job's only address,
  registered in `ClientHub` next to `CredStoreClientV1` and invoked by the
  host on an operator-chosen schedule; there is no REST route for it
- **`CredStorePluginClientV1`** — backend trait: a pure per-tenant, per-version
  value store (`get`/`put`/`delete` keyed by `tenant_id`/`value_id`); it holds
  no sharing/hierarchy/policy — that lives in the gear
- **`Credential`** / **`Secret`** — the two representations `get`/`get_secret`
  return: `Credential` never carries the value (`reference`, `secret_type`,
  `sharing`, `fallback`, `status`, `inheritance`, `version`, `updated_at`,
  `expires_at`, `validator`); `Secret` carries only what is needed to use the
  value (`reference`, `secret_type`, `expires_at`, `value`, `validator`)
- **`CredentialListItem`** — `list`'s item shape: a `Credential` plus an
  optional `secret`, populated only in value mode
- **`GcReport`** — `run_gc`'s outcome (`expired_deleted`, `gc_deleted`,
  `gc_pending_reclaimed`)
- **`CredentialWrite`** / **`CredentialPatch`** — `put`/`patch` request
  shapes; `CredentialPatch`'s `expires_at`/`value` are `PatchField<T>`
  (`Absent`/`Null`/`Set`), the RFC 7396 tri-state
- **`PutPrecondition`** / **`WritePrecondition`** — `put` distinguishes
  create-only (`If-None-Match: *`) from a guarded/unconditional replace;
  `patch`/`delete` keep the simpler `Exists`/`Matches` shape
- **`Fallback`** / **`CredentialStatus`** / **`InheritanceStatus`** —
  suppression policy and the two independent status fields of a `Credential`
- **`SecretRef`** / **`SecretValue`** / **`SharingMode`** / **`Validator`** —
  further domain models
- **`CredStoreError`** — Error types for all operations
- **`CredStorePluginSpecV1`** — GTS schema for plugin registration

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
    let secret = credstore.get_secret(security, &key).await?;

    Ok(secret.map(|s| s.value.as_bytes().len()))
}
```

A missing or out-of-scope credential is expressed as `Ok(None)`, preventing existence
leaks. An explicit denial of the `read_secret` action is returned as `CredStoreError::AccessDenied`.

## License

Apache-2.0
