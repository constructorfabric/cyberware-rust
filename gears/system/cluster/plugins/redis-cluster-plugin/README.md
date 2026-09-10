# cf-redis-cluster-plugin

The Redis backend plugin for the `cluster` gear: a native `ClusterCacheBackend` over
a `fred` connection pool plus a native `DistributedLockBackend` over a `SET NX PX`
lease with Lua-fenced renew and release. Recommended for high-throughput cache and
lock deployments, paired with a linearizable backend (K8s Lease) for leader election.

**Status: feature-complete.** Both primitives and both providers are implemented and
registered, so `provider: redis` under either `cache` or `lock` resolves in any build
of the cluster gear. Unit, conformance and integration tests all run under
`make test-cluster-redis`; [`docs/TESTING.md`](./docs/TESTING.md) §7 is the register of
what that covers. Both the Sentinel and the 3-node Cluster fixture are
built and run on every PR; there is no fault-injection layer, and
[`docs/TESTING.md`](./docs/TESTING.md) §8 registers what that leaves unverified.

This backend declares `EventuallyConsistent` in every replicated or non-fsync-durable
configuration, per ADR-009. A profile binding `cache: { provider: redis }` and
omitting `leader_election` or `lock` fails startup by design; see
[`docs/DESIGN.md`](./docs/DESIGN.md) §7 for the three supported ways out.

See [`docs/DESIGN.md`](./docs/DESIGN.md) for the full design and
[`docs/TESTING.md`](./docs/TESTING.md) for the test plan.
