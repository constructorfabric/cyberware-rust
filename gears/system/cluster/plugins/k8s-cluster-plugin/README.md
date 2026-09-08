# cf-k8s-cluster-plugin

Kubernetes backend plugin for the cluster gear. Provides **three native
primitives**:

- a `LeaderElectionBackend` over one `coordination.k8s.io/v1.Lease` per election,
- a `DistributedLockBackend` over one `Lease` per lock name (token-fenced), and
- a `ClusterCacheBackend` over a purpose-built `ClusterCacheEntry` custom resource.

It is the intended `leader_election` / `lock` binding for a Redis-cache profile
(ADR-009 rates the K8s Lease API safe with no configuration caveat), and makes the
single-provider "K8s, low-throughput" deployment shape expressible with zero new
infrastructure beyond a one-time CRD install.

See [`docs/DESIGN.md`](docs/DESIGN.md), [`docs/TESTING.md`](docs/TESTING.md)
