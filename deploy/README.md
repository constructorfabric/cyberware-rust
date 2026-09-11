# CF/Gears Profile 3 (Kubernetes) Deployment

A thin, end-to-end deployment guide for the out-of-process (OoP) gear
architecture. Every gear runs as its **own pod**; all external traffic enters
through the **platform-host** edge, which discovers pods via the DirectoryService
and reverse-proxies to them.

### Pods

- **platform-host** — one pod running the trust-coupled core (authz-resolver,
  tenant-resolver, resource-group, account-management) + system gears
  (gear-orchestrator, types-registry, credstore, **api-gateway** edge,
  **grpc-hub** DirectoryService) + embedded authn-resolver.
- **hello** — **anonymous** REST gear (no auth, no DB): the minimal cross-pod
  reverse-proxy case.
- **users-info** — **authenticated + remote-authz + Postgres**: the full OoP
  path (authenticate the bearer locally → central authz-resolver over REST → own DB).
- **api-contracts** / **api-contracts-consumer** — an **OoP→OoP pair**: the
  consumer pod calls the provider pod over REST (`PaymentApi`), discovered via
  the DirectoryService (the consumer binary does not link the provider).
- **shared-postgres** — one PostgreSQL pod serving a **database per gear**
  (`postgres` chart); gears never share tables — cross-gear reads go through SDK
  contracts, not SQL.

### Request paths

```
# 1) Anonymous (no token)
curl :8087/hello/v1/ping ──▶ platform-host (api-gateway edge) ──▶ hello pod
                             discovers via grpc-hub DirectoryService └▶ {"served_by":"hello-oop (pid 1)"}

# 2) Authenticated + remote authz + DB
curl -H 'Authorization: Bearer <jwt>' :8087/users-info/v1/cities
        └▶ edge (forwards the bearer) ──▶ users-info pod
                                            │ 1. authenticate the bearer locally
                                            │ 2. PEP ──▶ authz-resolver REST (back to platform-host) ──▶ allow + tenant scope
                                            │ 3. query ──▶ shared-postgres pod (usersinfo db)
                                            └▶ {"items":[ ... ]}

# 3) OoP → OoP (gear-to-gear over REST)
curl -H 'Authorization: Bearer <jwt>' :8087/api-contracts-consumer/v1/charge
        └▶ edge ──▶ api-contracts-consumer pod ──▶ api-contracts provider pod (REST) ──▶ {"payment_id":"...","status":"pending"}

# Underlying every arrow above:
#  • tenant-plane : a missing bearer is rejected with 401 at the edge
#                   (static-authn accept_all maps any non-empty bearer to the platform-root tenant)
#  • platform-plane: pods ↔ DirectoryService carry x-toolkit-internal-token, validated by K8s TokenReview
#  • discovery     : each pod self-registers; the edge syncs its route table from grpc-hub
```

## Layout

| Path | What |
|------|------|
| `apps/platform-host` | Platform-host binary crate (host mode). |
| `examples/toolkit/hello/hello` | `hello` gear + `hello-oop` OoP binary (`--features oop_module`). |
| `deploy/docker/platform-host.Dockerfile` | Platform-host image. |
| `deploy/docker/oop-gear.Dockerfile` | Generic per-gear OoP image (parameterized by build args). |
| `deploy/helm/toolkit-common` | Helm **library** chart (Deployment/Service/ConfigMap/SA + SA-token projection). |
| `deploy/helm/platform-host` | Platform-host chart. |
| `deploy/helm/hello` | `hello` OoP-gear chart. |
| `examples/toolkit/users-info/users-info` | `users-info` gear; OoP binary `users-info-oop` is a feature-gated `[[bin]]` (`--features oop_module`) in the gear crate. |
| `deploy/helm/users-info` | `users-info` OoP-gear chart (connects to the shared Postgres; can also bundle its own via `postgres.enabled`). |
| `examples/toolkit/api-contracts/api-contracts` | `api-contracts` PaymentApi REST **provider**; its OoP binary (`api-contracts-oop`) is a feature-gated `[[bin]]` (`--features oop_module`) in the gear crate. |
| `examples/toolkit/api-contracts/api-contracts-consumer` | `api-contracts-consumer`; its OoP binary (`api-contracts-consumer-oop`, feature `oop_module`) resolves `PaymentApi` from the provider **pod** over REST (OoP gear-to-gear). |
| `deploy/helm/api-contracts` | `api-contracts` provider OoP-gear chart. |
| `deploy/helm/api-contracts-consumer` | `api-contracts-consumer` OoP-gear chart. |
| `deploy/helm/postgres` | Shared PostgreSQL chart — one server, a database per gear (created by an init script). |
| `deploy/helm/toolkit-platform` | Umbrella chart (platform-host + gears) with `values-{dev,minimal,production}.yaml`. |

## Prerequisites

- Docker
- `minikube` + `kubectl`
- `helm`

## 1. Build images

```bash
# Platform-host (dev profile = faster build; drop --build-arg for optimized release).
# CARGO_FEATURES="k8s" compiles the grpc-hub inbound TokenReview validator.
DOCKER_BUILDKIT=1 docker build \
  -f deploy/docker/platform-host.Dockerfile \
  --build-arg BUILD_PROFILE=dev \
  --build-arg CARGO_FEATURES="k8s" \
  -t ghcr.io/constructorfabric/platform-host:dev .

# hello OoP gear (generic per-gear Dockerfile)
DOCKER_BUILDKIT=1 docker build \
  -f deploy/docker/oop-gear.Dockerfile \
  --build-arg GEAR_PACKAGE=hello \
  --build-arg GEAR_BIN=hello-oop \
  --build-arg GEAR_FEATURES="oop_module,k8s-auth" \
  --build-arg GEAR_CONFIG=config/oop-hello.yaml \
  --build-arg BUILD_PROFILE=dev \
  -t ghcr.io/constructorfabric/hello:dev .

# users-info OoP gear (authenticated + DB). The OoP binary is a feature-gated
# [[bin]] in the gear crate, so GEAR_PACKAGE is the gear crate and GEAR_FEATURES
# enables oop_module (+ k8s-auth for the platform plane).
DOCKER_BUILDKIT=1 docker build \
  -f deploy/docker/oop-gear.Dockerfile \
  --build-arg GEAR_PACKAGE=users-info \
  --build-arg GEAR_BIN=users-info-oop \
  --build-arg GEAR_FEATURES="oop_module,k8s-auth" \
  --build-arg GEAR_CONFIG=config/oop-users-info.yaml \
  --build-arg BUILD_PROFILE=dev \
  -t ghcr.io/constructorfabric/users-info:dev .

# api-contracts PaymentApi REST PROVIDER (authenticated, no DB). The OoP binary
# is a feature-gated [[bin]] in the gear crate (no separate -oop crate), so
# GEAR_PACKAGE is the gear crate and GEAR_FEATURES enables oop_module.
DOCKER_BUILDKIT=1 docker build \
  -f deploy/docker/oop-gear.Dockerfile \
  --build-arg GEAR_PACKAGE=cf-api-contracts \
  --build-arg GEAR_BIN=api-contracts-oop \
  --build-arg GEAR_FEATURES="oop_module,k8s-auth" \
  --build-arg GEAR_CONFIG=config/oop-api-contracts.yaml \
  --build-arg BUILD_PROFILE=dev \
  -t ghcr.io/constructorfabric/api-contracts:dev .

# api-contracts-consumer (authenticated, no DB) — calls the provider POD over REST.
# Also a feature-gated [[bin]] in the gear crate.
DOCKER_BUILDKIT=1 docker build \
  -f deploy/docker/oop-gear.Dockerfile \
  --build-arg GEAR_PACKAGE=cf-api-contracts-consumer \
  --build-arg GEAR_BIN=api-contracts-consumer-oop \
  --build-arg GEAR_FEATURES="oop_module,k8s-auth" \
  --build-arg GEAR_CONFIG=config/oop-api-contracts-consumer.yaml \
  --build-arg BUILD_PROFILE=dev \
  -t ghcr.io/constructorfabric/api-contracts-consumer:dev .
```

## 2. Load images into the cluster

```bash
minikube start                    # if not already running
minikube image load ghcr.io/constructorfabric/platform-host:dev
minikube image load ghcr.io/constructorfabric/hello:dev
minikube image load ghcr.io/constructorfabric/users-info:dev
minikube image load ghcr.io/constructorfabric/api-contracts:dev
minikube image load ghcr.io/constructorfabric/api-contracts-consumer:dev
```

> **Docker-driver gotcha:** `minikube image load <tag>` may **not** overwrite an
> existing tag if a running container still references the old image (you'll see
> the gear boot the *previous* build). If you rebuild an image, either delete the
> gear's pod first (`kubectl -n cf-gears delete pod -l
> app.kubernetes.io/name=<gear>`) or load via a tarball:
> `docker save <tag> -o /tmp/img.tar && minikube image load /tmp/img.tar`, then
> `kubectl -n cf-gears rollout restart deploy/<gear>`.

## 3. Deploy

```bash
kubectl create namespace cf-gears
minikube addons enable ingress                     # nginx controller for the edge Ingress
helm dependency build deploy/helm/platform-host
helm dependency build deploy/helm/hello
helm dependency build deploy/helm/users-info
helm dependency build deploy/helm/api-contracts
helm dependency build deploy/helm/api-contracts-consumer
helm dependency update deploy/helm/toolkit-platform # packages the sub-charts

helm upgrade --install platform deploy/helm/toolkit-platform \
  -n cf-gears \
  -f deploy/helm/toolkit-platform/values-dev.yaml \
  --timeout 240s

kubectl -n cf-gears get pods
```

## 4. Smoke test (edge → OoP)

All external traffic enters through the api-gateway edge, exposed by the
`platform-host` Ingress (`values-dev.yaml` enables it at host
`platform-host.local`, class `nginx`). Enable the controller once with
`minikube addons enable ingress`, then reach the edge without editing
`/etc/hosts` via `curl --resolve`:

```bash
MIP=$(minikube ip)
BASE="http://platform-host.local"
RESOLVE="--resolve platform-host.local:80:$MIP"

curl -s $RESOLVE $BASE/healthz            # platform-host edge -> 200
curl -s $RESOLVE $BASE/hello/v1/ping      # reverse-proxied to the hello pod
# => {"message":"pong","served_by":"hello-oop (pid 1)"}

# users-info: authenticated + remote-authz + Postgres, all through the edge.
# static-authn accept_all maps any non-empty bearer to the platform-root tenant.
TID=00000000-df51-5b42-9538-d2b56b7ee953

curl -s -o /dev/null -w '%{http_code}\n' \
  $RESOLVE $BASE/users-info/v1/cities                 # no token  -> 401

curl -s -X POST -H 'Authorization: Bearer test-token' -H 'Content-Type: application/json' \
  -d "{\"name\":\"Tokyo\",\"country\":\"JP\",\"tenant_id\":\"$TID\"}" \
  $RESOLVE $BASE/users-info/v1/cities                 # -> 201 Created

curl -s -H 'Authorization: Bearer test-token' \
  $RESOLVE $BASE/users-info/v1/cities                 # -> {"items":[{"name":"Tokyo",...}]}
```

The `$RESOLVE` / `$BASE` variables set here are reused by the commands in the
rest of this guide.

For `hello`, `served_by` is the serving process — proof the request was proxied
across pods. For `users-info`, the `201`/`200` responses prove the full OoP
path: the edge forwarded the bearer to the `users-info` pod, which authenticated
it locally, called the central `authz-resolver` **over REST** for the PEP
decision (visible as `POST /authz-resolver/v1/evaluate` in the platform-host
logs, sourced from the users-info pod IP), and persisted to its own Postgres.


## Platform-plane auth (TokenReview)

The two-plane model separates **tenant-plane** auth (end-user `Authorization:
Bearer` → `SecurityContext`, authenticated at each gear) from **platform-plane**
auth (service-to-service, `x-toolkit-internal-token`). This deployment enforces
the platform plane end-to-end using Kubernetes `TokenReview`.

**How it works**

- Each pod (platform-host + every OoP gear) mounts a **projected ServiceAccount
  token** with audience `toolkit-internal` at
  `/var/run/secrets/tokens/toolkit-internal/token` (`saToken.enabled` in the
  charts).
- Every gRPC **caller** of the DirectoryService attaches that token:
  - OoP gears via `oop_http.internal_auth: { provider: kube, token_path: ... }`.
  - the edge api-gateway proxy via `gateway_proxy.internal_auth`.
- The DirectoryService **receiver** (grpc-hub, in platform-host) validates every
  non-exempt RPC via the K8s `TokenReview` API:
  `grpc-hub.internal_auth: { provider: kube, audiences: [toolkit-internal] }`
  with `internal_auth_enforcement: required`. Health + reflection RPCs are exempt.
- `templates/rbac.yaml` in the platform-host chart binds its ServiceAccount to
  the built-in `system:auth-delegator` ClusterRole so it may submit
  TokenReviews. The `k8s` / `k8s-auth` cargo features compile the TokenReview
  code path (see [Build images](#1-build-images)).

**Verify enforcement**

```bash
# Positive: platform-host logs enforcement + accepted registrations.
kubectl -n cf-gears logs deploy/platform-host | grep "platform-plane enforcement enabled"
kubectl -n cf-gears logs deploy/platform-host | grep "registering gear proxy routes"

# Negative: a caller without a valid token is rejected. Temporarily remove a
# gear's oop_http.internal_auth (e.g. users-info) and redeploy; the gear
# cannot register and stays NotReady:
kubectl -n cf-gears logs deploy/users-info | grep "Unauthenticated"
# => "directory register_instance failed: gRPC Unauthenticated: missing internal token"
```

> The local loopback path below runs Profile-1-style (no projected tokens); it
> leaves `internal_auth` unset, so grpc-hub runs the pass-through layer. Use the
> `shared_secret` provider to exercise the platform plane without Kubernetes.

## OoP gear-to-gear (REST)

The `hello` and `users-info` paths above show OoP→**host** calls (users-info
resolves the in-host `authz-resolver` over REST). The `api-contracts` pair shows
OoP→**OoP** — one gear pod calling another gear pod over REST, discovered via the
DirectoryService:

- **`api-contracts`** (provider pod) serves the `PaymentApi` REST contract at
  `/api-contracts/v1/...` and registers its endpoint in the DirectoryService.
- **`api-contracts-consumer`** (consumer pod) exposes
  `POST /api-contracts-consumer/v1/charge`. Its handler resolves `dyn PaymentApi`
  from the ClientHub — wired by `#[toolkit::consumes(contract = PaymentApi, from
  = "api-contracts")]` to a **directory-resolving REST client** — and forwards
  the charge. The consumer binary does **not** link the provider, so the call can
  only travel over REST to the provider pod.

The consumer's `/charge` route is `.exposed()`, so a single request through the
edge exercises the whole chain — **ingress → api-gateway → consumer pod →
provider pod** (the consumer→provider hop travels OoP→OoP over REST):

```bash
curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST -H 'Content-Type: application/json' \
  -d '{"amount_cents":1000,"currency":"USD","description":"test"}' \
  $RESOLVE $BASE/api-contracts-consumer/v1/charge          # no token -> 401

curl -s -X POST -H 'Authorization: Bearer test-token' -H 'Content-Type: application/json' \
  -d '{"amount_cents":1000,"currency":"USD","description":"test charge"}' \
  $RESOLVE $BASE/api-contracts-consumer/v1/charge          # -> {"payment_id":"...","status":"pending"}
```

Confirm the hop crossed pods (the provider actually executed the charge):

```bash
kubectl -n cf-gears logs deploy/api-contracts | grep '"method":"charge"'
# => "contract call started" / "contract call succeeded" service=PaymentApi method=charge
kubectl -n cf-gears logs deploy/api-contracts-consumer | grep 'dependency resolved'
# => readiness: dependency resolved dep=api-contracts   (the resolving REST client)
```

## Local (no Kubernetes) end-to-end

Two processes on loopback, same software path:

```bash
# Terminal 1 — platform-host (edge + DirectoryService on TCP :50051)
cargo run -p cf-gears-platform-host -- --config config/oop-host.yaml run

# Terminal 2 — hello OoP gear
TOOLKIT_DIRECTORY_ENDPOINT=http://127.0.0.1:50051 \
  cargo run -p hello --features oop_module --bin hello-oop -- --config config/oop-hello.yaml

# Terminal 2b (optional) — users-info OoP gear (authenticated + DB).
# The local config uses per-pod SQLite (no Postgres needed on loopback);
# in Kubernetes the chart points it at a Postgres pod instead.
TOOLKIT_DIRECTORY_ENDPOINT=http://127.0.0.1:50051 \
  cargo run -p users-info --features oop_module --bin users-info-oop -- --config config/oop-users-info.yaml

# Terminal 3 — through the local edge (platform-host binds :8087)
curl http://127.0.0.1:8087/hello/v1/ping

curl -H 'Authorization: Bearer test-token' http://127.0.0.1:8087/users-info/v1/cities
```

## Helm presets

| Values file | Contents |
|-------------|----------|
| `values-dev.yaml` | platform-host + shared Postgres + 4 OoP gears (`hello`, `users-info`, `api-contracts`, `api-contracts-consumer`), `pullPolicy: Never` for locally-built images (Postgres uses the public image). Enables the `platform-host` Ingress (`platform-host.local`, class `nginx`). |
| `values-minimal.yaml` | platform-host only (no OoP gears). |
| `values-production.yaml` | Scaffold for a registry-pulled prod stack. Enables the `platform-host` Ingress with a placeholder host + TLS block to fill in. |

Only the `platform-host` chart defines an Ingress; OoP gears are reached through
its api-gateway by path prefix.

## Adding another OoP gear

First make the gear OoP-capable — OoP binary, own REST surface, platform-plane
auth, embedded authn stack (if authenticated), and DB isolation. Those are the
authoring requirements, covered in
[`docs/arch/toolkit-oop/GEAR_REQUIREMENTS.md`](../docs/arch/toolkit-oop/GEAR_REQUIREMENTS.md).
Then wire it into this deployment:

1. Build the image via `deploy/docker/oop-gear.Dockerfile` (`GEAR_PACKAGE` = the
   gear crate, `GEAR_BIN` = the OoP bin, `GEAR_FEATURES="oop_module,k8s-auth"`).
2. Copy `deploy/helm/hello` (or `deploy/helm/users-info` if it needs a DB) as a
   template; adjust `service.port`, `config.content` (`oop_http.advertise_uri` +
   `gears.<name>` + any `database` block), and the image.
3. Add it to the `toolkit-platform` umbrella `Chart.yaml` dependencies + values.
   For a DB-backed gear, add its database to `postgres.databases` and point the
   gear's `postgres` block at `host: shared-postgres` (see §8 of the
   requirements).

## Cleanup

```bash
helm -n cf-gears uninstall platform
kubectl delete namespace cf-gears
# minikube stop   # or: minikube delete
```
