//! Shared harness for `tests/standalone_integration_tests.rs`
//! (design.md D8, task group 10): spawns the real, compiled
//! `cf-gears-event-broker-server` binary as a subprocess against a
//! temp-file `SQLite` DB and drives it over real HTTP - unlike the in-crate
//! `EventBrokerHarness` (`test_support/harness.rs`), which stubs the type
//! registry and bypasses the platform gear lifecycle entirely. This is the
//! only way to exercise `toolkit::bootstrap`'s real `pre_init`/`init`/
//! `post_init`/start-phase sequencing, the same sequencing whose ordering
//! bugs (task 9) no mock-backed test could have caught.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cf-gears-event-broker-server"))
}

/// Binds an ephemeral port and immediately releases it - standard test
/// practice; the small TOCTOU window before the child binds it is an
/// accepted risk, same as every other ephemeral-port test in this repo.
fn alloc_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Splits declarative fixtures into what each side of the process receives.
///
/// A test writes what it means - a topic with a partition count, an event type
/// with a topic and a payload contract - and this puts each half where the
/// running process expects it: the topic and event type become the documents
/// `types-registry` holds (an instance, and a derived type schema of the
/// abstract event base), while the partition count becomes `event-broker`
/// configuration, because a topic carries no count of its own.
///
/// An event-type fixture may name `partition_key` to point the type at a member
/// other than the tenant the base defaults to.
fn split_fixtures(entities: &[Value]) -> (Vec<Value>, Vec<(String, i64)>) {
    let topic_base = "gts.cf.core.events.topic.v1~";
    let event_base = "gts.cf.core.events.event.v1~";
    let mut documents = Vec::new();
    let mut partitions = Vec::new();

    for entity in entities {
        let id = entity["id"]
            .as_str()
            .expect("a fixture entity names its id");
        if id.starts_with(topic_base) && !id.ends_with('~') {
            partitions.push((
                id.to_owned(),
                entity["partitions"]
                    .as_i64()
                    .expect("a topic fixture names its partition count"),
            ));
            documents.push(serde_json::json!({
                "id": id,
                "description": "a topic this integration test publishes to",
            }));
        } else if id.starts_with(event_base) {
            let allowed: Vec<&str> = entity["allowed_subject_types"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .map(|value| value.as_str().expect("a subject-type pattern is a string"))
                        .collect()
                })
                .unwrap_or_default();
            let mut schema = event_broker_sdk::gts::derived_event_type_schema(
                id,
                entity["topic_id"]
                    .as_str()
                    .expect("an event-type fixture names its topic"),
                entity
                    .get("data_schema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                &allowed,
            );
            if let Some(pointer) = entity.get("partition_key").and_then(Value::as_str) {
                schema["x-gts-traits"]["partition_key"] = serde_json::json!(pointer);
            }
            documents.push(schema);
        } else {
            panic!("fixture '{id}' is neither a topic instance nor an event type");
        }
    }

    (documents, partitions)
}

/// Builds the `types-registry` `entities:` list as a YAML flow sequence of
/// JSON-object literals (YAML is a superset of JSON for values) - avoids
/// hand-crafting indentation-sensitive block-style YAML per entity.
fn config_yaml(home_dir: &Path, port: u16, entities: &[Value]) -> String {
    let (documents, partitions) = split_fixtures(entities);
    let entities_json: Vec<String> = documents
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let topic_settings: String = partitions
        .iter()
        .map(|(id, count)| format!("        \"{id}\":\n          partitions: {count}\n"))
        .collect();
    format!(
        r#"
server:
  home_dir: "{home}"

database:
  servers: {{}}

gears:
  types-registry:
    config:
      entities: [{entities}]

  cluster:
    config:
      profiles:
        event-broker:
          cache:
            provider: standalone

  authn-resolver:
    config:
      vendor: "constructorfabric"

  authz-resolver:
    config:
      vendor: "constructorfabric"

  static-authn-plugin:
    config:
      vendor: "constructorfabric"
      priority: 100
      mode: accept_all

  static-authz-plugin:
    config:
      vendor: "constructorfabric"
      priority: 100

  event-broker:
    database:
      engine: sqlite
      file: "event_broker.db"
    config:
      mode: standalone
      default_storage_backend: "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~"
      # The event log lives in the backend's own database, beside the gear's
      # rather than inside it - the gear's keeps ingest and delivery metadata.
      # A file rather than `:memory:` because the restart test asserts events
      # survive the process that wrote them.
      topics:
        "gts.cf.core.events.topic.v1~":
          backend:
            type: "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~"
            path: "{home}/event-broker/event_log.db"
{topic_settings}

  api-gateway:
    config:
      bind_addr: "127.0.0.1:{port}"
      enable_docs: false
      auth_disabled: true
"#,
        home = home_dir.display(),
        entities = entities_json.join(", "),
        topic_settings = topic_settings,
    )
}

fn drain<R: AsyncRead + Unpin + Send + 'static>(reader: R, logs: &Arc<Mutex<Vec<String>>>) {
    let logs = Arc::clone(logs);
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            logs.lock().expect("logs mutex").push(line);
        }
    });
}

fn spawn_server(
    home_dir: &Path,
    port: u16,
    entities: &[Value],
) -> (Child, Arc<Mutex<Vec<String>>>) {
    let config_path = home_dir.join("config.yaml");
    std::fs::write(&config_path, config_yaml(home_dir, port, entities)).expect("write config");

    let mut child = Command::new(bin_path())
        .arg("--config")
        .arg(&config_path)
        .arg("run")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn cf-gears-event-broker-server");

    let logs = Arc::new(Mutex::new(Vec::new()));
    drain(child.stdout.take().expect("stdout was piped"), &logs);
    drain(child.stderr.take().expect("stderr was piped"), &logs);

    (child, logs)
}

/// Polls `GET /healthz` (unauthenticated, unprefixed - `api-gateway`'s main
/// router merges health routes outside the auth layer) until it answers
/// `200`, the process exits early, or 15s elapse. Captured stdout/stderr is
/// included in the panic message either way, so a boot failure is
/// diagnosable without re-running under `-- --nocapture`.
async fn wait_for_healthy(child: &mut Child, base_url: &str, logs: &Arc<Mutex<Vec<String>>>) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let captured = logs.lock().expect("logs mutex").join("\n");
            panic!(
                "server exited early ({status}) before becoming healthy; captured output:\n{captured}"
            );
        }
        if let Ok(resp) = client.get(format!("{base_url}/healthz")).send().await
            && resp.status().is_success()
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let captured = logs.lock().expect("logs mutex").join("\n");
            panic!("server never became healthy within 15s; captured output:\n{captured}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A running `cf-gears-event-broker-server` subprocess, bound to an
/// ephemeral port and a temp-file `SQLite` DB under a per-test `home_dir`.
pub struct TestServer {
    child: Child,
    logs: Arc<Mutex<Vec<String>>>,
    base_url: String,
    home_dir: TempDir,
    entities: Vec<Value>,
}

impl TestServer {
    /// Boots a fresh server. `entities` seeds `types-registry`'s startup
    /// config (the same `entities:` mechanism `config/event-broker-
    /// standalone.yaml` documents) - typically topic/event-type fixtures.
    pub async fn start(entities: Vec<Value>) -> Self {
        let home_dir = TempDir::new().expect("tempdir");
        let port = alloc_port();
        let (mut child, logs) = spawn_server(home_dir.path(), port, &entities);
        let base_url = format!("http://127.0.0.1:{port}");
        wait_for_healthy(&mut child, &base_url, &logs).await;
        Self {
            child,
            logs,
            base_url,
            home_dir,
            entities,
        }
    }

    /// Kills the process (`SIGKILL`, not a graceful shutdown - `run_server`
    /// owns its own internal `CancellationToken` with no external hook, and
    /// an abrupt kill is the more realistic "restart after crash" scenario
    /// anyway) and boots a fresh one against the SAME `home_dir` - and
    /// therefore the same `SQLite` file - on a new ephemeral port. For task
    /// 10.3's durability test: data must survive; subscription state must
    /// not (it lives in the `ClusterCacheV1` standalone in-memory profile,
    /// which dies with the process).
    pub async fn restart(&mut self) {
        self.child.kill().await.expect("kill for restart");
        let _ = self.child.wait().await;
        let port = alloc_port();
        let (mut child, logs) = spawn_server(self.home_dir.path(), port, &self.entities);
        let base_url = format!("http://127.0.0.1:{port}");
        wait_for_healthy(&mut child, &base_url, &logs).await;
        self.child = child;
        self.logs = logs;
        self.base_url = base_url;
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Reads SSE events (`event: <kind>\ndata: <json>\n\n`) off a real
/// `GET .../events:sse` response one at a time, buffering across however
/// `reqwest`'s `bytes_stream()` happens to chunk the underlying TCP socket -
/// unlike the in-crate router tests (`streaming_tests.rs`), a real socket
/// gives no guarantee that one `Frame` arrives as exactly one read, or that
/// two frames don't arrive in the same read.
pub struct SseFrameReader {
    stream:
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: String,
}

impl SseFrameReader {
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            buf: String::new(),
        }
    }

    pub async fn next_frame(&mut self, timeout: Duration) -> (String, Value) {
        use tokio_stream::StreamExt as _;

        tokio::time::timeout(timeout, async {
            loop {
                if let Some(end) = self.buf.find("\n\n") {
                    let raw = self.buf[..end].to_owned();
                    self.buf.drain(..=end + 1);
                    let event = raw
                        .strip_prefix("event: ")
                        .expect("SSE block must start with 'event: '");
                    let (kind, data) = event
                        .split_once("\ndata: ")
                        .expect("SSE block must contain a 'data: ' line");
                    let parsed: Value = serde_json::from_str(data).expect("SSE data must be JSON");
                    return (kind.to_owned(), parsed);
                }
                let chunk = self
                    .stream
                    .next()
                    .await
                    .expect("SSE stream must not end before a full frame arrives")
                    .expect("SSE chunk must not be a transport error");
                self.buf.push_str(&String::from_utf8_lossy(&chunk));
            }
        })
        .await
        .expect("must receive an SSE frame within the timeout")
    }

    /// Non-panicking variant of `next_frame`: `None` if no complete frame
    /// arrives within `timeout`, or the connection closes - used to assert
    /// the ABSENCE of a frame (e.g. proving a partition lost in a rebalance
    /// stops being delivered), where `next_frame`'s panic-on-timeout would
    /// be the wrong tool.
    pub async fn try_next_frame(&mut self, timeout: Duration) -> Option<(String, Value)> {
        use tokio_stream::StreamExt as _;

        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let raw = self.buf[..end].to_owned();
                self.buf.drain(..=end + 1);
                let event = raw
                    .strip_prefix("event: ")
                    .expect("SSE block must start with 'event: '");
                let (kind, data) = event
                    .split_once("\ndata: ")
                    .expect("SSE block must contain a 'data: ' line");
                let parsed: Value = serde_json::from_str(data).expect("SSE data must be JSON");
                return Some((kind.to_owned(), parsed));
            }
            match tokio::time::timeout(timeout, self.stream.next()).await {
                Ok(Some(Ok(chunk))) => self.buf.push_str(&String::from_utf8_lossy(&chunk)),
                Ok(Some(Err(_))) => panic!("SSE chunk must not be a transport error"),
                Ok(None) | Err(_) => return None,
            }
        }
    }
}
