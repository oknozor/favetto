//! End-to-end daemon tests.
//!
//! These spawn the real `favetto` binary against isolated temp dirs and a Unix
//! socket, then attach a raw MessagePack wire client — the same length-prefixed
//! framing the TUI uses — and drive the daemon over RPC. A fake "echo" agent runs
//! a catalog task to completion, and the WebSocket surface is checked for its
//! bearer-token rejection.
//!
//! The `favetto` crate has no library target (it is a single binary), so the
//! client here is a deliberately small reimplementation of the wire framing over
//! [`favetto_core::wire::FrameCodec`] rather than a reuse of `favetto::client`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::rpc::{method, push, Frame, Notification, Request, Response};
use favetto_core::wire::FrameCodec;
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::codec::Framed;

/// How long the daemon may take to start accepting connections.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Bound on a single RPC round trip / awaited notification.
const RPC_TIMEOUT: Duration = Duration::from_secs(20);

/// An isolated daemon instance plus its scratch workspace. Dropping the harness
/// kills the child and removes everything it wrote.
struct DaemonHarness {
    root: PathBuf,
    socket: PathBuf,
    listen: String,
    log_path: PathBuf,
    child: Child,
}

impl DaemonHarness {
    /// Create a scratch workspace with a fake echo agent and a one-task catalog,
    /// then spawn `favetto daemon` against it.
    fn spawn() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("favetto-daemon-e2e-{}-{nanos}", std::process::id()));
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        // A fake agent that prints the prompt it was handed ($1) and a
        // deterministic marker, then exits 0.
        let agent = root.join("echo-agent.sh");
        std::fs::write(
            &agent,
            "#!/bin/sh\nprintf 'echo-agent: %s\\n' \"$1\"\nprintf 'ECHO_AGENT_DONE\\n'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        std::fs::write(
            tasks_dir.join("echo_task.md"),
            "agent = \"echo\"\n---\necho: hello-e2e\n",
        )
        .unwrap();

        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"echo\"\n\n\
                 [agents.echo]\ntype = \"configurable\"\ncommand = {:?}\n\
                 headless_args = [\"{{prompt}}\"]\n\n\
                 [executor]\ndetect_awaiting_input = false\n",
                agent.to_string_lossy()
            ),
        )
        .unwrap();

        let socket = root.join("favetto.sock");
        let listen = format!("127.0.0.1:{}", free_port());
        let log_path = root.join("daemon.log");
        let log = std::fs::File::create(&log_path).unwrap();

        let child = Command::new(env!("CARGO_BIN_EXE_favetto"))
            .arg("daemon")
            .arg("--config")
            .arg(&config_path)
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--tasks-dir")
            .arg(&tasks_dir)
            .arg("--socket")
            .arg(&socket)
            .arg("--listen")
            .arg(&listen)
            .current_dir(&root)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .env_remove("FAVETTO_CONFIG")
            .env_remove("FAVETTO_DATA_DIR")
            .spawn()
            .expect("spawn favetto daemon");

        Self {
            root,
            socket,
            listen,
            log_path,
            child,
        }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn listen(&self) -> &str {
        &self.listen
    }

    /// The captured daemon log, for assertion failure messages.
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A free loopback port. The listener is dropped immediately, so a small race
/// remains; acceptable for a test and far cheaper than parsing the daemon log.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Block until the Unix socket accepts connections, or panic with the daemon log.
async fn wait_for_unix_socket(path: &Path, daemon: &DaemonHarness) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match UnixStream::connect(path).await {
            Ok(_) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!(
                "daemon socket {} never became ready: {e}\n--- daemon log ---\n{}",
                path.display(),
                daemon.log()
            ),
        }
    }
}

/// Block until the HTTP/WebSocket listener accepts connections, or panic.
async fn wait_for_tcp(listen: &str, daemon: &DaemonHarness) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::TcpStream::connect(listen).await {
            Ok(_) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!(
                "daemon HTTP listener {listen} never became ready: {e}\n--- daemon log ---\n{}",
                daemon.log()
            ),
        }
    }
}

/// A minimal MessagePack wire client over the local Unix socket.
struct WireClient {
    framed: Framed<UnixStream, FrameCodec>,
    next_id: u64,
}

impl WireClient {
    async fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket)
            .await
            .expect("connect unix socket");
        Self {
            framed: Framed::new(stream, FrameCodec),
            next_id: 1,
        }
    }

    /// Read the next frame, bounded by [`RPC_TIMEOUT`].
    async fn next_frame(&mut self) -> Frame {
        tokio::time::timeout(RPC_TIMEOUT, self.framed.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("connection closed before a frame arrived")
            .expect("frame decode error")
    }

    /// Send a request and return its matching response, skipping notifications.
    async fn request(&mut self, method: &str, params: serde_json::Value) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        self.framed
            .send(Frame::Request(Request {
                id,
                method: method.to_string(),
                params,
            }))
            .await
            .expect("send request");
        loop {
            match self.next_frame().await {
                Frame::Response(resp) if resp.id == id => return resp,
                Frame::Notification(_) => continue,
                other => panic!("unexpected frame while awaiting response {id}: {other:?}"),
            }
        }
    }

    /// Read until a notification matching `pred` arrives.
    async fn notification<F>(&mut self, mut pred: F) -> Notification
    where
        F: FnMut(&Notification) -> bool,
    {
        loop {
            match self.next_frame().await {
                Frame::Notification(n) if pred(&n) => return n,
                _ => continue,
            }
        }
    }
}

#[tokio::test]
async fn wire_client_pings_lists_catalog_and_runs_a_task() {
    let daemon = DaemonHarness::spawn();
    wait_for_unix_socket(daemon.socket(), &daemon).await;

    let mut client = WireClient::connect(daemon.socket()).await;

    // `system.ping` proves the MessagePack handshake and dispatch work.
    let pong = client.request(method::PING, serde_json::json!({})).await;
    assert!(pong.error.is_none(), "system.ping failed: {:?}", pong.error);
    assert_eq!(pong.result.unwrap()["pong"], serde_json::json!(true));

    // `catalog.list` sees the fake catalog task from disk.
    let catalog = client
        .request(method::CATALOG_LIST, serde_json::json!({}))
        .await
        .result
        .expect("catalog.list result");
    let catalog = catalog.as_array().expect("catalog.list is an array");
    assert!(
        catalog
            .iter()
            .any(|t| t["name"] == "echo_task" && t["agent"] == "echo"),
        "catalog.list did not include the fake echo task: {catalog:?}"
    );

    // `tasks.start` enqueues it and returns the pending row.
    let started = client
        .request(
            method::TASKS_START,
            serde_json::json!({ "name": "echo_task" }),
        )
        .await;
    assert!(
        started.error.is_none(),
        "tasks.start failed: {:?}",
        started.error
    );
    let task = started.result.expect("tasks.start result");
    let task_id = task["id"].as_str().expect("task id").to_string();
    assert_eq!(task["status"], "pending");

    // The executor runs it through the fake agent and emits `task_finished`.
    let finished = client
        .notification(|n| {
            n.method == push::EVENT
                && n.params["kind"] == "task_finished"
                && n.params["payload"]["name"] == "echo_task"
        })
        .await;
    assert_eq!(
        finished.params["payload"]["success"],
        serde_json::json!(true),
        "the fake echo agent should have succeeded: {finished:?}"
    );

    // The persisted row is terminal and carries the agent's output.
    let got = client
        .request(method::TASKS_GET, serde_json::json!({ "id": task_id }))
        .await
        .result
        .expect("tasks.get result");
    assert_eq!(got["status"], "succeeded");
    let output = got["output"]["output"].as_str().expect("task output text");
    assert!(
        output.contains("echo-agent: echo: hello-e2e"),
        "the fake agent did not echo the rendered prompt: {output:?}"
    );
    assert!(
        output.contains("ECHO_AGENT_DONE"),
        "the fake agent's completion marker is missing: {output:?}"
    );
}

#[tokio::test]
async fn websocket_rejects_a_wrong_bearer_token() {
    let daemon = DaemonHarness::spawn();
    wait_for_tcp(daemon.listen(), &daemon).await;

    // Build a well-formed upgrade request (the standard handshake headers) and
    // then attach a bearer token the daemon does not know.
    let url = format!("ws://{}/rpc", daemon.listen());
    let mut request = url.into_client_request().expect("build websocket request");
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        "Bearer not-the-daemon-token".parse().unwrap(),
    );

    match tokio_tungstenite::connect_async(request).await {
        Ok(_) => panic!("a wrong bearer token must be rejected before the upgrade"),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(
                resp.status().as_u16(),
                401,
                "expected a 401 rejection, got {}",
                resp.status()
            );
        }
        Err(other) => panic!("expected an HTTP 401 rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn websocket_accepts_a_single_use_ticket() {
    let daemon = DaemonHarness::spawn();
    wait_for_tcp(daemon.listen(), &daemon).await;
    let token = std::fs::read_to_string(daemon.root.join("data/token"))
        .expect("daemon token")
        .trim()
        .to_string();

    // Without a bearer, the ticket endpoint refuses to hand one out.
    let http = reqwest::Client::new();
    let unauthorized = http
        .post(format!("http://{}/auth/ticket", daemon.listen()))
        .send()
        .await
        .expect("POST /auth/ticket");
    assert_eq!(unauthorized.status().as_u16(), 401);

    let body: serde_json::Value = http
        .post(format!("http://{}/auth/ticket", daemon.listen()))
        .bearer_auth(&token)
        .send()
        .await
        .expect("POST /auth/ticket")
        .error_for_status()
        .expect("a valid bearer issues a ticket")
        .json()
        .await
        .expect("ticket response is JSON");
    let ticket = body["ticket"].as_str().expect("ticket string").to_string();
    assert_eq!(body["expires_in"], serde_json::json!(30));

    // The ticket upgrades the WebSocket exactly once.
    let socket =
        tokio_tungstenite::connect_async(format!("ws://{}/rpc?ticket={ticket}", daemon.listen()))
            .await
            .expect("a fresh ticket upgrades the WebSocket");
    drop(socket);

    // Replaying the same ticket is rejected before the upgrade.
    match tokio_tungstenite::connect_async(format!("ws://{}/rpc?ticket={ticket}", daemon.listen()))
        .await
    {
        Ok(_) => panic!("a reused ticket must be rejected"),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(
                resp.status().as_u16(),
                401,
                "expected a 401 rejection, got {}",
                resp.status()
            );
        }
        Err(other) => panic!("expected an HTTP 401 rejection, got {other:?}"),
    }
}

/// The TUI's remote attach uses the `favetto-tui` client (not the raw test
/// client above). A base URL without `/rpc` must still reach the daemon, which
/// only upgrades WebSockets at `/rpc`.
#[tokio::test]
async fn websocket_remote_attach_accepts_a_base_url() {
    use favetto_tui::client::{Client, Transport};

    let daemon = DaemonHarness::spawn();
    wait_for_tcp(daemon.listen(), &daemon).await;
    let token = std::fs::read_to_string(daemon.root.join("data/token"))
        .expect("daemon token")
        .trim()
        .to_string();

    // A base URL with no path: the client must append `/rpc` for the upgrade.
    let client = Client::connect(Transport::Ws {
        url: format!("ws://{}", daemon.listen()),
        token: Some(token),
    })
    .await
    .expect("remote attach over a base WebSocket URL");

    let resp = client
        .request(method::TASKS_LIST, serde_json::json!({}))
        .await
        .expect("rpc round trip over the WebSocket");
    assert!(
        resp.error.is_none(),
        "tasks.list failed over the WebSocket: {:?}",
        resp.error
    );
    assert!(resp.result.is_some(), "tasks.list returned no result");
}
