# cf-gears-quota-enforcement-sdk

Public, transport-agnostic contract of the `quota-enforcement` gear.

## What this crate provides

- `QuotaEnforcementStoragePluginV1`: the storage plugin contract. One trait,
  a closed `StorageError` enum, and the thirteen invariants (I1 to I13) that
  every implementation upholds.
- `CoordinationPluginV1`: the coordination plugin contract. Three methods
  (`try_lock`, `renew`, `release`), the closed `LockScope` enum, the opaque
  `Lock` token, and the closed `CoordinationError` enum.
- Domain types and closed enums that the two contracts reference: `Quota`,
  `QuotaSnapshot`, `DebitPlan`, `Decision`, `IdempotencyScope`,
  `NotificationEvent`, `MutationResult`, policy records, and pagination types.
- GTS plugin specs used for discovery: `QuotaEnforcementStoragePluginSpecV1`
  and `QuotaEnforcementCoordinationPluginSpecV1`.
- GTS resource identifiers for the canonical error envelope.

The consumer, manager, and operator client traits land with their features.
This crate ships the plugin side first, so plugin authors implement against a
single dependency.

## Test support

Enable the `test-util` feature to get `quota_enforcement_sdk::testing`. It
holds complete in-memory doubles of both plugin contracts. The doubles are
for tests only.

## Design source

`gears/system/quota-enforcement/docs/DESIGN.md`, section 3.3, defines the
contracts. `docs/features/foundation.md` defines the foundation scope.
