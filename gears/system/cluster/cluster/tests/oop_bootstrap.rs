//! `D1`: the `cluster-oop` binary, spawned as a process and asked the questions the
//! exit criteria ask (DESIGN.md).
//!
//! `tests/oop_probe_ordering.rs` covers the *ordering* property in-process, where a
//! gate gear can hold the start phase open and the cancellation token is an
//! argument. This file covers what only a real process can: that `cluster-oop
//! --config <file>` - clap, `#[tokio::main]`, `registered_gears.rs`'s linkage, YAML
//! loading, logging init, the eager `DirectoryService` connect and
//! `build_oop_serve_options` - comes up and serves `/healthz`, `/readyz`, `/health`
//! and `/openapi.json`, then drains on SIGTERM.
//!
//! The binary is located through `CARGO_BIN_EXE_cluster-oop`, so cargo builds it
//! before this target runs and the path can never be a stale artefact.
//!
//! **A `DirectoryService` is mandatory, and that is the framework's constraint, not
//! a fixture convenience**: `run_oop_with_options` calls
//! `DirectoryGrpcClient::connect(...).await?` and propagates the failure
//! (`bootstrap/oop.rs:551-553`), so no `OoP` gear starts without a directory. That is
//! open ask `A2`, and it is why this test serves a stub one on an ephemeral port
//! rather than pointing the child at the default `127.0.0.1:50051`.
//!
//! **What this file deliberately does not do** is add a consumer, resolve a facade
//! across the socket, or assert cluster DNS naming - that is `D4`, gated on `D2`.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a setup failure IS the test failure"
)]

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use cf_system_sdks::directory::{
    DeregisterInstanceRequest, DirectoryService, DirectoryServiceServer, GetOpenApiSpecRequest,
    GetOpenApiSpecResponse, HeartbeatRequest, ListAllInstancesRequest, ListAllInstancesResponse,
    ListInstancesRequest, ListInstancesResponse, RegisterInstanceRequest,
    ResolveGrpcServiceRequest, ResolveGrpcServiceResponse, ResolveRestServiceRequest,
    ResolveRestServiceResponse,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tonic::{Request, Response, Status};

// A stub DirectoryService, over a real socket

/// Accepts presence, resolves nothing.
///
/// Resolution failing is the honest shape for this test: cluster consumes no
/// contract, so nothing should ask. Presence calls (register / heartbeat /
/// deregister) succeed but are not observed: whether the framework's presence loop
/// actually registers and deregisters is the framework's own contract, covered by
/// `toolkit::runtime::oop_registration_tests`, not something a gear re-proves
/// (ADR-0005). This test needs a directory only so the eager
/// `DirectoryGrpcClient::connect` in `run_oop_with_options` succeeds.
struct StubDirectory;

#[tonic::async_trait]
impl DirectoryService for StubDirectory {
    async fn resolve_grpc_service(
        &self,
        _request: Request<ResolveGrpcServiceRequest>,
    ) -> Result<Response<ResolveGrpcServiceResponse>, Status> {
        Err(Status::not_found("stub directory resolves nothing"))
    }

    async fn resolve_rest_service(
        &self,
        _request: Request<ResolveRestServiceRequest>,
    ) -> Result<Response<ResolveRestServiceResponse>, Status> {
        Err(Status::not_found("stub directory resolves nothing"))
    }

    async fn get_open_api_spec(
        &self,
        _request: Request<GetOpenApiSpecRequest>,
    ) -> Result<Response<GetOpenApiSpecResponse>, Status> {
        Err(Status::not_found("stub directory resolves nothing"))
    }

    async fn list_instances(
        &self,
        _request: Request<ListInstancesRequest>,
    ) -> Result<Response<ListInstancesResponse>, Status> {
        Ok(Response::new(ListInstancesResponse { instances: vec![] }))
    }

    async fn list_all_instances(
        &self,
        _request: Request<ListAllInstancesRequest>,
    ) -> Result<Response<ListAllInstancesResponse>, Status> {
        Ok(Response::new(ListAllInstancesResponse {
            instances: vec![],
        }))
    }

    async fn register_instance(
        &self,
        _request: Request<RegisterInstanceRequest>,
    ) -> Result<Response<()>, Status> {
        Ok(Response::new(()))
    }

    async fn deregister_instance(
        &self,
        _request: Request<DeregisterInstanceRequest>,
    ) -> Result<Response<()>, Status> {
        Ok(Response::new(()))
    }

    async fn heartbeat(&self, _request: Request<HeartbeatRequest>) -> Result<Response<()>, Status> {
        Ok(Response::new(()))
    }
}

/// Serve the stub on an ephemeral port, returning its endpoint.
async fn serve_stub_directory() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let service = DirectoryServiceServer::new(StubDirectory);
    tokio::spawn(async move {
        // The task is dropped with the test, which closes the listener; a serve
        // error at that point is teardown, not a failure.
        drop(
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await,
        );
    });

    format!("http://{addr}")
}

// The child process

/// Reserve an ephemeral port and release it, so the child can bind it.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// The operator YAML a `cluster-oop` pod is given.
///
/// Three sections carry the weight. `oop_http` is what makes the bootstrap take
/// the HTTP-serving lifecycle at all - without it the probes do not exist, because
/// `run_oop_with_options` falls back to its legacy gRPC-only path. `gears.grpc-hub`
/// is the `RegistryError::GrpcRequiresHub` requirement in config form: the hub
/// must be linked *and* configured.
/// `gears.cluster` is one standalone profile, so `start` needs no Docker and no
/// network (§7.6).
///
/// `allow_loopback_advertise` is required because the advertise URI defaults to
/// the listen address, and bootstrap now rejects a loopback advertise URI as
/// registered-but-unreachable under multi-host Profile 2 / Profile 3. This test is
/// exactly the single-host case the flag is for.
fn config_yaml(home_dir: &Path, http_port: u16, grpc_port: u16) -> String {
    format!(
        "server:\n  \
           home_dir: {home}\n\
         oop_http:\n  \
           listen_addr: \"127.0.0.1:{http_port}\"\n  \
           allow_loopback_advertise: true\n  \
           drain_timeout_secs: 5\n  \
           healthcheck_timeout_ms: 500\n\
         gears:\n  \
           grpc-hub:\n    \
             config:\n      \
               listen_addr: \"127.0.0.1:{grpc_port}\"\n  \
           cluster:\n    \
             config:\n      \
               profiles:\n        \
                 orders:\n          \
                   cache: {{ provider: standalone }}\n\
         logging:\n  \
           default:\n    \
             console_level: info\n",
        home = home_dir.display(),
    )
}

/// A raw HTTP/1.1 GET returning `(status, body)`.
async fn http_get(addr: SocketAddr, path: &str) -> Option<(u16, String)> {
    let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())?;
    let body = text.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
    Some((status, body))
}

/// Poll until `path` answers `want` with a body `accept` likes.
///
/// The body predicate matters for the same reason it does in
/// `oop_probe_ordering.rs`: `/readyz` and `/health` read through a 2 s report cache
/// (`healthcheck.rs:217`), so a status alone can be a stale answer.
async fn await_body(
    addr: SocketAddr,
    path: &str,
    want: u16,
    accept: impl Fn(&str) -> bool,
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::from("<never connected>");
    loop {
        if let Some((status, body)) = http_get(addr, path).await {
            if status == want && accept(&body) {
                return body;
            }
            last = format!("{status} {body}");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "GET {path} never settled on {want} with an acceptable body within {timeout:?}; last: {last}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Send SIGTERM, which is the signal a pod's drain actually uses.
///
/// `Child::kill` is SIGKILL and would prove nothing about the drain, and no signal
/// crate is a dependency here - so this shells out, which is also exactly what a
/// `preStop` hook does.
fn sigterm(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill should be available on a unix host");
    assert!(status.success(), "SIGTERM to {pid} failed");
}

// The test

/// Reaps the child on *every* exit from the test, an unwinding assertion included.
///
/// `std::process::Child` has no `Drop` that kills or waits, so a panic in the probe
/// block below would otherwise unwind straight past the manual reap and orphan a
/// `cluster-oop` still bound to both ports - which nextest reports as `FAIL + LEAK`
/// (the leaked child keeps the test's stdout/stderr pipe open). Owning the child
/// through this guard makes the reap a `Drop`, so it happens-before every exit,
/// panic or not.
struct ChildReaper(std::process::Child);

impl Drop for ChildReaper {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            drop(self.0.kill());
            drop(self.0.wait());
        }
    }
}

/// `cargo build -p cf-gears-cluster --bin cluster-oop` and
/// `cluster-oop --config <file>` serving the four probes - `D1`'s first and third
/// exit criteria, through the binary rather than around it.
#[tokio::test(flavor = "multi_thread")]
async fn cluster_oop_binary_serves_the_framework_probes_and_drains_on_sigterm() {
    let directory_endpoint = serve_stub_directory().await;

    let home = tempfile::tempdir().expect("tempdir");
    let http_port = free_port();
    let grpc_port = free_port();
    let config_path = home.path().join("cluster-oop.yaml");
    std::fs::write(&config_path, config_yaml(home.path(), http_port, grpc_port))
        .expect("write config");

    let probe_addr: SocketAddr = format!("127.0.0.1:{http_port}").parse().unwrap();

    let mut child = ChildReaper(
        std::process::Command::new(env!("CARGO_BIN_EXE_cluster-oop"))
            .arg("--config")
            .arg(&config_path)
            // The bootstrap reads the endpoint from this variable when the option is
            // defaulted, which is how a master host hands it down (`OopRunOptions`).
            // Setting it here, on the child only, keeps the test off the well-known
            // 50051 and out of the parent's environment.
            .env(
                toolkit::runtime::TOOLKIT_DIRECTORY_ENDPOINT_ENV,
                &directory_endpoint,
            )
            .env("HOME", home.path())
            .spawn()
            .expect("cluster-oop should spawn - cargo builds it for this target"),
    );

    // The probe block below asserts, and an assertion panics rather than returns;
    // `child` is a `ChildReaper` so that unwind still reaps the process on the way
    // out. A bare `std::process::Child` would be dropped without a kill, orphaning
    // a `cluster-oop` on both ports - the `FAIL + LEAK` nextest reports.
    let outcome = tokio::time::timeout(Duration::from_secs(90), async {
        // 1. Liveness: the listener binds and answers before anything else is true.
        let body = await_body(
            probe_addr,
            "/healthz",
            200,
            |_| true,
            Duration::from_mins(1),
        )
        .await;
        assert_eq!(body.trim(), "ok", "/healthz body");

        // 2. Readiness reaches `Ready`, and it is cluster's composite check saying
        //    so: the component name only appears once `compose_oop_router` has
        //    collected `RestApiCapability::healthcheck`, which happens after the
        //    gear's `start` published its profiles (§4.4).
        let health = await_body(
            probe_addr,
            "/health",
            200,
            |body| body.contains("cluster-readiness"),
            Duration::from_secs(30),
        )
        .await;
        assert!(
            health.contains("\"status\":\"healthy\""),
            "cluster's composite check should be healthy over one standalone profile, got: {health}"
        );

        let readyz = await_body(
            probe_addr,
            "/readyz",
            200,
            |body| body.contains("\"state\":\"ready\""),
            Duration::from_secs(30),
        )
        .await;
        assert!(
            readyz.contains("\"ready\":true"),
            "/readyz should mirror the Ready state, got: {readyz}"
        );

        // 3. The OpenAPI document is published. Cluster registers no routes yet
        //    (`S4` adds the admin plane), so the assertion is that the *document*
        //    serves and is titled for this gear - not that it has paths.
        let (status, spec) = http_get(probe_addr, "/openapi.json")
            .await
            .expect("connects");
        assert_eq!(status, 200, "/openapi.json; body: {spec}");
        assert!(
            spec.contains("\"openapi\""),
            "/openapi.json should serve an OpenAPI document, got: {spec}"
        );
        let (status, well_known) = http_get(probe_addr, "/.well-known/openapi.json")
            .await
            .expect("connects");
        assert_eq!(status, 200, "the canonical discovery path should serve too");
        assert_eq!(
            well_known, spec,
            "both OpenAPI paths are the same handler and must not diverge"
        );

        // Presence is deliberately NOT asserted here. Whether the bootstrap's
        // registration loop runs is the framework's contract (ADR-0005), covered in
        // `toolkit::runtime::oop_registration_tests`; re-proving it from a gear
        // couples this test to an asynchronous, readiness-decoupled path (§4.4) it
        // has no business observing. This test proves the gear-specific half only -
        // that `cluster-oop` boots, links its gears, and serves the probes.
    })
    .await;

    // 4. Drain on SIGTERM: `/readyz` must go 503 `draining` and the process must
    //    exit 0. This is the half a `preStop` delay exists to make useful (§4.8,
    //    `D2`), so it is asserted here rather than assumed.
    let drain = if outcome.is_ok() {
        sigterm(child.0.id());
        let exit = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(status) = child.0.try_wait().expect("try_wait") {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        Some(exit)
    } else {
        None
    };

    // No manual reap here: `child` is a `ChildReaper`, so it is reaped when it drops
    // at end of scope - and, crucially, when an assertion below unwinds past it.

    outcome.expect("cluster-oop should come up and serve its probes within 90s");
    let exit = drain
        .expect("startup succeeded, so the drain must have been attempted")
        .expect("cluster-oop should exit within 30s of SIGTERM");
    assert!(
        exit.success(),
        "cluster-oop should exit cleanly after SIGTERM, got {exit:?}"
    );
}

// ADR-0005's confirmation step, mechanised

/// `D1`'s fourth exit criterion: **no registration or dependency retry loop
/// appears in cluster's own code** (§4.3, ADR-0005).
///
/// The criterion names a code review, and the review is the real artefact - but a
/// review does not survive the next edit, so this pins the two shapes it looked
/// for. Directory registration, heartbeats, backoff and dependency resolution are
/// the bootstrap's, and a gear that grew its own would be re-implementing the
/// platform's presence model in a place nothing audits.
///
/// Scoped to the binary's own sources on purpose. The gear crate at large has
/// plenty of legitimate loops (reapers, watch pumps, the conformance suite); what
/// must be absent is a *registration or dependency* retry, and the vocabulary for
/// one is narrow enough to name.
#[test]
fn no_registration_or_dependency_retry_loop_in_the_binary() {
    let sources = [
        ("src/main.rs", include_str!("../src/main.rs")),
        (
            "src/registered_gears.rs",
            include_str!("../src/registered_gears.rs"),
        ),
    ];

    // Words that would only appear if this binary had taken over presence or
    // dependency resolution. Matched against code, not prose: the doc comments in
    // `main.rs` discuss retry loops precisely in order to say there are none, so
    // comment lines are stripped first.
    let forbidden = [
        "register_instance",
        "send_heartbeat",
        "deregister_instance",
        "resolve_rest_service",
        "resolve_grpc_service",
        "DirectoryClient",
        "backoff",
        "sleep",
        "loop",
        "retry",
    ];

    for (name, source) in sources {
        let code: String = source
            .lines()
            .map(str::trim_start)
            .filter(|line| !line.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        for needle in forbidden {
            assert!(
                !code.contains(needle),
                "{name} contains `{needle}` outside a comment: ADR-0005 requires that \
                 registration, heartbeat, backoff and dependency resolution stay in the \
                 bootstrap and appear nowhere in a gear's own code"
            );
        }
    }
}
