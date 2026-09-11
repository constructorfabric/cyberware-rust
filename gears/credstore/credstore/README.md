# `CredStore`

Stateful credential-storage gear module. Owns per-secret metadata in its own
database, enforces authorization in SQL, resolves secrets hierarchically across
the tenant tree, and stores the secret **value** in a backend plugin discovered
via the types registry.

> Design: the [technical design](https://github.com/constructorfabric/gears-rust/blob/main/gears/credstore/docs/DESIGN.md) is the baseline; the decision to
> ship a stateful gear (`credstore_secrets` table, PDP-scope authz,
> versioning/ETag, write saga) instead of the original stateless design is
> recorded in [ADR-0001](https://github.com/constructorfabric/gears-rust/blob/main/gears/credstore/docs/ADR/0001-cpt-cf-credstore-adr-stateful-gear.md);
> the addendum document that used to carry this history was folded into that
> ADR and removed (see git history for its prior content).

## Overview

The `cf-gears-credstore` module provides:

- **Local metadata** — a gear-owned `credstore_secrets` table (`SecureORM` /
  sea-orm, migration `m0001`) holding sharing, owner, status, `version`, and
  (ADR-0006) `value_id`, the pointer to the row's current immutable backend
  version, plus a `credstore_value_gc` table tracking versions pending
  reclaim
- **Credential record / secret value split** (ADR-0004): the record
  (`reference`, `type`, `sharing`, `fallback`, `status`, `inheritance`,
  `version`, `owner_id`) and its value are two representations, each with
  its own read address — `GET /credentials/{ref}` never carries the value,
  only `GET /credentials/{ref}/secret` does — and one write address: `PUT`
  creates or replaces both together, `PATCH` (RFC 7396 JSON Merge Patch)
  edits the record, rotates the value, or removes it (`{"value": null}`)
  without recreating anything
- **PDP authorization** — six actions on the resolved concrete type
  (`list`/`read`/`write`/`delete` on the record, `read_secret`/
  `write_secret` on the value; `crate::gts::permissions`), `AccessScope`
  enforced in SQL via `SecureORM` clamps; out-of-scope access is fail-closed
  (canonical 404, anti-enumeration)
- **Hierarchical resolution** — a single indexed query over the ancestor chain
  (TTL+LRU cached, barriers ignored — `shared` inherits through them); the backend is read once for the winner's value
- **Upward-rooted collection read** (ADR-0005): `GET /credentials` lists one
  reduced record per reference, rooted at the caller's tenant and its
  ancestor chain only, filterable/orderable on an indexed allowlist and
  keyset-paginated; selecting `secret` in `$select` switches the same
  collection into a capped, non-paginated **value mode** that returns the
  decrypted value alongside each item the caller may read
- **Value-fingerprint fence** — every read verifies the backend value against a
  per-row `HMAC-SHA256` (key auto-stored in the backend, never on the wire), so
  a metadata/value desync from a concurrent write fails closed instead of
  disclosing a value under a foreign sharing label (DESIGN §4.10, ADR-0003)
  (integrity check; no healing path, since ADR-0006 recreates the value
  outright when something looks wrong)
- **Versioning** — strong generation-bound `ETag` (`"<id>.<version>"`) on `GET`,
  mandatory `If-Match` on `PATCH`/`DELETE` (and, for `PUT`, exactly one of
  `If-None-Match: *` create-only or `If-Match`); a validator, or `*` for
  explicit last-writer-wins; no ABA across recreation
- **Immutable value versions** (ADR-0006) — every write mints a fresh
  version under a new id; the row's pointer switches to it in one
  transaction only after the bytes are durably written; the version it
  replaced is deleted by the writer right after the switch — that is the
  cleanup path; leftovers (a failed writer-side delete, or a crash) and
  expired records are removed by a periodic maintenance job, not an
  in-process reaper: there is no resident timer in the gear at all
- **`CredStoreMaintenanceV1`** — the maintenance job's only entry point:
  `run_gc` is an in-process `ClientHub` trait, not a REST route, invoked by
  the host (a `gc` subcommand under a Kubernetes `CronJob`, or a scheduler
  gear) on an operator-chosen schedule
- **Backend plugin** — value-only store discovered via the types registry (vendor)
- **`ClientHub` + REST** — registers `CredStoreClientV1`; exposes `/credstore/v1/credentials`

This module depends on `types-registry`, `tenant-resolver`, and `authz-resolver`,
and **requires a database**. The secret value is stored in a plugin (e.g.
`cf-gears-static-credstore-plugin`, or an OpenBao-backed plugin).

## Usage

After the gear initializes, consumers obtain its client from `ClientHub`. This
example reads a secret value (`get_secret`, the `read_secret` action) without
formatting or logging it; the record itself is read with `get` (`read`):

```no_run
use std::error::Error;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit::ClientHub;
use toolkit_security::SecurityContext;

async fn secret_length(
    hub: &ClientHub,
    security: &SecurityContext,
) -> Result<Option<usize>, Box<dyn Error>> {
    let credstore = hub.get::<dyn CredStoreClientV1>()?;
    let key = SecretRef::new("my-api-key")?;
    let secret = credstore.get_secret(security, &key).await?;

    Ok(secret.map(|s| s.value.as_bytes().len()))
}
```

## Configuration

The module requires a `database:` section (it is stateful). Gear config:

```yaml
credstore:
  database:
    server: "sqlite_users"   # a database server template; module gets its own file
    file: "credstore.db"
  config:
    vendor: "constructorfabric" # GTS vendor used to discover the value-store plugin
    hierarchy:
      ancestor_cache_ttl_secs: 300
    gc:                          # settings of the maintenance job (ADR-0006); no resident reaper
      pending_max_age_secs: 3600 # a pending write intent older than this is reclaimed by the job
      batch_size: 256            # rows per batch in the job's expiry and gc passes
    list:                        # GET /credentials (ADR-0005/ADR-0004)
      max_limit: 200             # metadata-mode page-size cap; a caller `limit` above this is 400 INVALID_LIMIT
      value_mode_cap: 25         # match-set cap for a value-mode ($select=…,secret) request; over this is 400 TOO_MANY_MATCHES
```

## License

Apache-2.0
