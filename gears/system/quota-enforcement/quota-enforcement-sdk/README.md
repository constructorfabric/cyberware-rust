# cf-gears-quota-enforcement-sdk

Public, transport-agnostic contract of the `quota-enforcement` gear.

## What this crate provides

- `QuotaEnforcementStoragePluginV1`: the storage plugin contract. One trait,
  a closed `StorageError` enum, and the thirteen invariants (I1 to I13) that
  every implementation upholds.
- Domain types and closed enums that the contract references: `Quota`,
  `QuotaSnapshot`, `DebitPlan`, `Decision`, `IdempotencyScope`,
  `NotificationEvent`, `MutationResult`, policy records, and pagination types.
- The GTS plugin spec used for discovery: `QuotaEnforcementStoragePluginSpecV1`.
- GTS resource identifiers for the canonical error envelope.

Singleton coordination for the sweepers is not a contract of this SDK. The
gear consumes the platform `cluster` gear's leader election, and the operator
selects the backend in the cluster profile YAML (ADR-0006).

The consumer, manager, and operator client traits land with their features.
This crate ships the plugin side first, so plugin authors implement against a
single dependency.

## Test support

Enable the `test-util` feature to get `quota_enforcement_sdk::testing`. It
holds a complete in-memory double of the storage plugin contract. The double
is for tests only.

## Design source

`gears/system/quota-enforcement/docs/DESIGN.md`, section 3.3, defines the
contract. `docs/features/foundation.md` defines the foundation scope.
