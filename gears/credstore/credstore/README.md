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
  the value-fingerprint fence (planned, ADR-0006: gains `value_id`, the
  pointer to the row's current immutable backend version)
- **PDP authorization** — `AccessScope` enforced in SQL via `SecureORM` clamps;
  out-of-scope access is fail-closed (canonical 404, anti-enumeration)
- **Hierarchical resolution** — a single indexed query over the ancestor chain
  (TTL+LRU cached, barriers ignored — `shared` inherits through them); the backend is read once for the winner's value
- **Value-fingerprint fence** — every read verifies the backend value against a
  per-row `HMAC-SHA256` (key auto-stored in the backend, never on the wire), so
  a metadata/value desync from a concurrent write fails closed instead of
  disclosing a value under a foreign sharing label (DESIGN §4.10, ADR-0003)
  (integrity check; no healing path under ADR-0006)
- **Versioning** — strong generation-bound `ETag` (`"<id>.<version>"`) on `GET`,
  mandatory `If-Match` on `PUT`/`DELETE` (a validator, or `*` for explicit
  last-writer-wins; no ABA across recreation)
- **Crash-safe writes** — each value is a new immutable version under a fresh
  id; the row's pointer switches in one transaction after the bytes are
  written; old versions are deleted by the writer right after the switch —
  that is the cleanup path; leftovers (a failed writer-side delete, or a
  crash) and expired records are removed by a periodic maintenance job, not
  an in-process reaper (planned, ADR-0006; the shipped saga —
  provisioning→backend→active with rollback and a reaper — until then)
- **Backend plugin** — value-only store discovered via the types registry (vendor)
- **`ClientHub` + REST** — registers `CredStoreClientV1`; exposes `/credstore/v1/secrets`

This module depends on `types-registry`, `tenant-resolver`, and `authz-resolver`,
and **requires a database**. The secret value is stored in a plugin (e.g.
`cf-gears-static-credstore-plugin`, or an OpenBao-backed plugin).

**Planned direction (not yet implemented).** [ADR-0004](https://github.com/constructorfabric/gears-rust/blob/main/gears/credstore/docs/ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md)
and [ADR-0005](https://github.com/constructorfabric/gears-rust/blob/main/gears/credstore/docs/ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md)
(both `proposed`) split the credential record and its secret value into two
addressable resources and add a metadata listing, upward-rooted through the
tenant hierarchy, that never carries a value unless the caller's `$select`
names `secret` — which switches that same collection read into a capped,
non-paginated bulk value read — and rename the PDP resource type to
`gts.cf.core.credstore.credential.v1~` and replace the three shipped actions
with six on it (`list`, `read`, `write`, `delete` on the record;
`read_secret`, `write_secret` on the value). Under that model a credential is
created in one request — a `PUT` on the record address carries the record
and its value together; a merge-`PATCH` on the same address edits metadata
or rotates/removes the value without touching the rest of the record; and
the value has its own read address, distinct from the record, plus the
collection's value mode for reading several at once. [ADR-0006](https://github.com/constructorfabric/gears-rust/blob/main/gears/credstore/docs/ADR/0006-cpt-cf-credstore-adr-immutable-value-versions.md)
(`proposed`) replaces in-place overwrite with immutable value versions — the model of Vault KV v2 and the cloud secret managers, shadow paging with a `git gc`-style collector — so the saga and its reaper go away: every
write mints a fresh version id, switches the row's pointer to it in one
transaction, and deletes the version it replaced right after — with a
periodic maintenance job, run on an operator-chosen schedule outside the
gear (no resident reaper), collecting whatever that best-effort delete
missed.

## Usage

After the gear initializes, consumers obtain its client from `ClientHub`. This
example retrieves a secret without formatting or logging its value:

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
    let response = credstore.get(security, &key).await?;

    Ok(response.map(|secret| secret.value.as_bytes().len()))
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
    reaper:
      tick_secs: 60
      provisioning_timeout_secs: 300
      # planned, ADR-0006: removed — replaced by the `gc` job
      # (`gc.pending_max_age_secs: 3600`, `gc.batch_size: 256`), run as
      # `credstore gc` on an operator-chosen schedule, not inside the gear
```

## License

Apache-2.0
