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
    /// Spawn a daemon, retrying if the ephemeral port is stolen before the
    /// daemon binds it (the child then exits and its scratch dir is removed).
    fn spawn() -> Self {
        Self::spawn_with(|_| Vec::new())
    }

    /// Spawn a daemon whose fake agent receives additional environment
    /// variables, computed from the scratch root so tests can point the agent
    /// at marker files inside it.
    fn spawn_with<F>(agent_env: F) -> Self
    where
        F: Fn(&Path) -> Vec<(String, String)>,
    {
        for _ in 0..8 {
            let mut daemon = Self::try_spawn(&agent_env, &[]);
            if daemon.wait_bound() {
                return daemon;
            }
            // `daemon` is dropped: kills the child and removes its scratch dir.
        }
        panic!("daemon HTTP listener never bound after 8 attempts");
    }

    /// Synchronously wait (up to `STARTUP_TIMEOUT`) for the TCP listener to
    /// accept; returns false if the child exited first (e.g. its port was
    /// stolen).
    fn wait_bound(&mut self) -> bool {
        let addr: std::net::SocketAddr = self.listen.parse().expect("listen is host:port");
        let deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
        loop {
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return true;
            }
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return false;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Create a scratch workspace with fake ACP agents and a catalog of one
    /// happy-path task plus the transport-fault tasks, then spawn
    /// `favetto daemon` against it.
    fn try_spawn<F>(agent_env: &F, daemon_env: &[(&str, &str)]) -> Self
    where
        F: Fn(&Path) -> Vec<(String, String)>,
    {
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

        // A deterministic ACP server that echoes the prompt it was handed and a
        // completion marker, then ends the turn.
        let agent = env!("CARGO_BIN_EXE_fake-acp-agent");

        std::fs::write(
            tasks_dir.join("echo_task.md"),
            "agent = \"echo\"\nprovider = \"fake\"\nmodel = \"model-b\"\n---\necho: hello-e2e\n",
        )
        .unwrap();
        // The transport-fault catalog. Each agent answers (or refuses to answer)
        // with a distinct malformed startup so the daemon's failure path can be
        // asserted end to end. `pid_file` lets a test prove the child was reaped.
        let pid_file = root.join("agent.pid");
        for name in ["broken_utf8", "oversized", "truncated", "hung"] {
            std::fs::write(
                tasks_dir.join(format!("{name}_task.md")),
                format!("agent = \"{name}\"\n---\necho: {name}\n"),
            )
            .unwrap();
        }

        let env_lines: String = agent_env(&root)
            .iter()
            .map(|(key, value)| format!("{key} = {value:?}\n"))
            .collect();
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"echo\"\n\n\
                 [agents.echo]\ncommand = {:?}\nargs = []\n\
                 [agents.echo.env]\n{env_lines}\n\
                 [agents.broken_utf8]\ncommand = {agent:?}\nargs = []\n\
                 [agents.broken_utf8.env]\nFAKE_ACP_INVALID_UTF8 = \"1\"\n\
                 FAKE_ACP_PID_FILE = {:?}\n\n\
                 [agents.oversized]\ncommand = {agent:?}\nargs = []\n\
                 [agents.oversized.env]\nFAKE_ACP_OVERSIZED = \"1\"\n\
                 FAKE_ACP_PID_FILE = {:?}\n\n\
                 [agents.truncated]\ncommand = {agent:?}\nargs = []\n\
                 [agents.truncated.env]\nFAKE_ACP_TRUNCATE = \"1\"\n\
                 FAKE_ACP_PID_FILE = {:?}\n\n\
                 [agents.hung]\ncommand = {agent:?}\nargs = []\n\
                 [agents.hung.env]\nFAKE_ACP_INIT_HANG = \"1\"\n\
                 FAKE_ACP_PID_FILE = {:?}\n",
                agent, pid_file, pid_file, pid_file, pid_file,
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
            .envs(daemon_env.iter().copied())
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

    /// Spawn a daemon with extra variables in the daemon's own environment.
    fn spawn_with_env(env: &[(&str, &str)]) -> Self {
        for _ in 0..8 {
            let mut daemon = Self::try_spawn(&|_| Vec::new(), env);
            if daemon.wait_bound() {
                return daemon;
            }
            // `daemon` is dropped: kills the child and removes its scratch dir.
        }
        panic!("daemon HTTP listener never bound after 8 attempts");
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

/// Point the fake agent at per-daemon close and PID markers.
fn marker_env(root: &Path) -> Vec<(String, String)> {
    vec![
        (
            "FAKE_ACP_CLOSE_MARKER".to_string(),
            root.join("close-marker").to_string_lossy().into_owned(),
        ),
        (
            "FAKE_ACP_PID_MARKER".to_string(),
            root.join("agent-pid").to_string_lossy().into_owned(),
        ),
    ]
}

/// Start the catalog's echo task, wait for it to finish, and return its row.
async fn run_echo_task(daemon: &DaemonHarness) -> serde_json::Value {
    wait_for_unix_socket(daemon.socket(), daemon).await;
    let mut client = WireClient::connect(daemon.socket()).await;
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
    let task_id = started.result.expect("tasks.start result")["id"]
        .as_str()
        .expect("task id")
        .to_string();
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
    client
        .request(method::TASKS_GET, serde_json::json!({ "id": task_id }))
        .await
        .result
        .expect("tasks.get result")
}

/// Wait for the fake agent to write its PID, then for the process to be gone.
/// `running` is cleared before the supervisor reaps the child, so this polls.
async fn wait_for_agent_exit(daemon: &DaemonHarness) {
    let pid_path = daemon.root.join("agent-pid");
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                break pid;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the fake agent never wrote its PID to {}",
            pid_path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    loop {
        // SAFETY: signal 0 only probes whether the process still exists.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        if !alive {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fake ACP agent {pid} survived session teardown\n--- daemon log ---\n{}",
            daemon.log()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn acp_session_close_is_sent_when_advertised() {
    let daemon = DaemonHarness::spawn_with(marker_env);
    let task = run_echo_task(&daemon).await;
    assert_eq!(task["status"], "succeeded");
    assert!(
        daemon.root.join("close-marker").exists(),
        "an agent that advertises session/close should receive it\n--- daemon log ---\n{}",
        daemon.log()
    );
    wait_for_agent_exit(&daemon).await;
}

#[tokio::test]
async fn acp_session_close_is_not_sent_when_not_advertised() {
    let daemon = DaemonHarness::spawn_with(|root| {
        let mut env = marker_env(root);
        env.push(("FAKE_ACP_CLOSE_ADVERTISED".to_string(), "0".to_string()));
        env
    });
    let task = run_echo_task(&daemon).await;
    assert_eq!(task["status"], "succeeded");
    assert!(
        !daemon.root.join("close-marker").exists(),
        "session/close must not be sent without an advertised capability\n--- daemon log ---\n{}",
        daemon.log()
    );
    wait_for_agent_exit(&daemon).await;
}

#[tokio::test]
async fn acp_session_close_times_out_without_blocking_teardown() {
    let daemon = DaemonHarness::spawn_with(|root| {
        let mut env = marker_env(root);
        env.push(("FAKE_ACP_IGNORE_CLOSE".to_string(), "1".to_string()));
        env
    });
    let task = run_echo_task(&daemon).await;
    assert_eq!(task["status"], "succeeded");
    assert!(
        daemon.root.join("close-marker").exists(),
        "an advertised close should still be attempted before timing out\n--- daemon log ---\n{}",
        daemon.log()
    );
    wait_for_agent_exit(&daemon).await;
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
    assert!(
        output.contains("model: fake/model-b"),
        "the task's provider/model was not selected over ACP: {output:?}"
    );
}

/// A malformed, oversized, hung, or truncated ACP server must fail the task with
/// a useful message and leave no child process behind. The setup deadline is
/// shortened through the daemon environment so the hung case is fast.
#[tokio::test]
async fn acp_transport_faults_fail_the_task_and_reap_the_agent() {
    let daemon = DaemonHarness::spawn_with_env(&[("FAVETTO_ACP_SETUP_TIMEOUT_MS", "500")]);
    wait_for_unix_socket(daemon.socket(), &daemon).await;
    let mut client = WireClient::connect(daemon.socket()).await;

    for (name, needle) in [
        ("broken_utf8_task", "UTF-8"),
        ("oversized_task", "exceeds"),
        ("truncated_task", "terminating newline"),
        ("hung_task", "did not complete within"),
    ] {
        let started = client
            .request(method::TASKS_START, serde_json::json!({ "name": name }))
            .await;
        assert!(
            started.error.is_none(),
            "tasks.start({name}) failed: {:?}",
            started.error
        );
        let task_id = started.result.expect("tasks.start result")["id"]
            .as_str()
            .expect("task id")
            .to_string();

        let finished = client
            .notification(|n| {
                n.method == push::EVENT
                    && n.params["kind"] == "task_finished"
                    && n.params["payload"]["name"] == name
            })
            .await;
        assert_eq!(
            finished.params["payload"]["success"],
            serde_json::json!(false),
            "{name} must fail: {finished:?}"
        );

        let row = client
            .request(method::TASKS_GET, serde_json::json!({ "id": task_id }))
            .await
            .result
            .expect("tasks.get result");
        assert_eq!(row["status"], "failed", "{name}: {row:?}");
        let message = row["failure"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            message.contains(needle),
            "{name} failure did not mention {needle:?}: {message:?}\n--- daemon log ---\n{}",
            daemon.log()
        );

        // Both the agent and its supervisor must be gone: the reaper killed the
        // process group and collected the direct child instead of leaving a
        // zombie.
        let pids = std::fs::read_to_string(daemon.root.join("agent.pid"))
            .unwrap_or_else(|error| panic!("{name}: no agent pid file: {error}\n{}", daemon.log()));
        let mut pids = pids.split_whitespace().map(|pid| {
            pid.parse::<i32>()
                .unwrap_or_else(|error| panic!("{name}: bad pid {pid:?}: {error}"))
        });
        let agent_pid = pids.next().expect("agent pid");
        let supervisor_pid = pids.next().expect("supervisor pid");
        for pid in [agent_pid, supervisor_pid] {
            wait_for_pid_gone(pid, name, &daemon);
        }
    }
}

/// Wait (bounded) for `pid` to stop existing. A zombie still answers `kill(0)`,
/// so this proves the process was reaped, not merely killed.
fn wait_for_pid_gone(pid: i32, name: &str, daemon: &DaemonHarness) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        // SAFETY: signal 0 only probes for existence; it delivers nothing.
        let exists = unsafe { libc::kill(pid, 0) } == 0;
        if !exists {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "{name}: agent pid {pid} still exists after the task failed\n--- daemon log ---\n{}",
                daemon.log()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
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

/// The embedded SPA is served over the real HTTP listener: `/` returns the
/// shell, an HTML navigation falls back to it, and a missing asset is a 404.
#[tokio::test]
async fn spa_is_served_over_http() {
    let daemon = DaemonHarness::spawn();
    wait_for_tcp(daemon.listen(), &daemon).await;
    let http = reqwest::Client::new();

    let index = http
        .get(format!("http://{}/", daemon.listen()))
        .send()
        .await
        .expect("GET /");
    assert_eq!(index.status().as_u16(), 200);
    assert_eq!(
        index
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        index
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );
    let body = index.text().await.expect("index body");
    // The embedded assets may be the committed placeholder or a local `dx`
    // build; both are the SPA's HTML entry point.
    assert!(
        body.trim_start()
            .to_ascii_lowercase()
            .starts_with("<!doctype html"),
        "{body}"
    );

    // An unmatched HTML navigation falls back to the shell.
    let fallback = http
        .get(format!("http://{}/inbox/abc", daemon.listen()))
        .header(reqwest::header::ACCEPT, "text/html")
        .send()
        .await
        .expect("GET /inbox/abc");
    assert_eq!(fallback.status().as_u16(), 200);
    let fallback_body = fallback.text().await.expect("fallback body");
    assert!(
        fallback_body
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("<!doctype html"),
        "{fallback_body}"
    );

    // A missing hashed asset is a 404, never the shell.
    let missing = http
        .get(format!("http://{}/assets/missing.js", daemon.listen()))
        .send()
        .await
        .expect("GET /assets/missing.js");
    assert_eq!(missing.status().as_u16(), 404);
}

/// systemd stops a service with SIGTERM. The daemon must exit cleanly
/// (status 0) instead of being killed by the signal, so the graceful shutdown
/// path runs.
#[tokio::test]
async fn sigterm_shuts_the_daemon_down_gracefully() {
    let mut daemon = DaemonHarness::spawn();
    wait_for_unix_socket(daemon.socket(), &daemon).await;

    // SAFETY: `kill` is async-signal-safe; the pid is a live child of this test.
    unsafe { libc::kill(daemon.child.id() as libc::pid_t, libc::SIGTERM) };

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        match daemon
            .child
            .try_wait()
            .expect("try_wait on the daemon child")
        {
            Some(status) => {
                assert!(
                    status.success(),
                    "daemon exited with {status} after SIGTERM; log:\n{}",
                    daemon.log()
                );
                assert!(
                    daemon.log().contains("shutdown signal received"),
                    "graceful shutdown path did not run; log:\n{}",
                    daemon.log()
                );
                break;
            }
            None if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            None => panic!(
                "daemon did not exit within 20s of SIGTERM; log:\n{}",
                daemon.log()
            ),
        }
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

/// Session setup preserves the complete ACP config-option/mode snapshot and the
/// daemon rejects changes the live agent did not advertise.
#[tokio::test]
async fn session_options_and_modes_are_exposed_validated_and_changed() {
    let daemon = DaemonHarness::spawn();
    wait_for_unix_socket(daemon.socket(), &daemon).await;
    let mut client = WireClient::connect(daemon.socket()).await;

    let started = client
        .request(method::AGENTS_START, serde_json::json!({ "agent": "echo" }))
        .await;
    assert!(
        started.error.is_none(),
        "agents.start failed: {:?}",
        started.error
    );
    let session_id = started.result.expect("agents.start result")["session"]["id"]
        .as_str()
        .expect("session id")
        .to_string();

    // The session starts asynchronously; poll until setup has published its
    // initial options/modes.
    let options = loop {
        let response = client
            .request(
                method::AGENTS_OPTIONS,
                serde_json::json!({ "session_id": session_id }),
            )
            .await;
        assert!(
            response.error.is_none(),
            "agents.options failed: {:?}",
            response.error
        );
        let result = response.result.expect("agents.options result");
        if result["config_options"]
            .as_array()
            .is_some_and(|options| !options.is_empty())
        {
            break result;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(options["config_options"][0]["id"], "model");
    assert_eq!(options["config_options"][0]["category"], "model");
    assert_eq!(options["config_options"][1]["category"], "thought_level");
    assert_eq!(options["modes"]["current_mode_id"], "build");
    assert_eq!(options["modes"]["available_modes"][1]["id"], "plan");

    // A select value the agent advertised is applied and the full snapshot is
    // reconciled, including the other options.
    let changed = client
        .request(
            method::AGENTS_SET_OPTION,
            serde_json::json!({
                "session_id": session_id,
                "option_id": "thought",
                "value": { "type": "value_id", "value": "high" }
            }),
        )
        .await;
    assert!(
        changed.error.is_none(),
        "agents.set_option failed: {:?}",
        changed.error
    );
    let changed = changed.result.expect("agents.set_option result");
    assert_eq!(
        changed["config_options"][1]["kind"]["current_value"],
        "high"
    );
    assert_eq!(
        changed["config_options"][0]["kind"]["current_value"],
        "fake/model-a"
    );

    // Boolean options are typed too.
    let boolean = client
        .request(
            method::AGENTS_SET_OPTION,
            serde_json::json!({
                "session_id": session_id,
                "option_id": "fast",
                "value": { "type": "boolean", "value": true }
            }),
        )
        .await;
    assert!(
        boolean.error.is_none(),
        "boolean set failed: {:?}",
        boolean.error
    );
    assert_eq!(
        boolean.result.expect("boolean result")["config_options"][2]["kind"]["current_value"],
        true
    );

    // Stale/unknown option ids and unadvertised values are rejected before the
    // agent is called.
    for params in [
        serde_json::json!({
            "session_id": session_id,
            "option_id": "stale",
            "value": { "type": "value_id", "value": "high" }
        }),
        serde_json::json!({
            "session_id": session_id,
            "option_id": "thought",
            "value": { "type": "value_id", "value": "impossible" }
        }),
        serde_json::json!({
            "session_id": session_id,
            "option_id": "fast",
            "value": { "type": "value_id", "value": "true" }
        }),
    ] {
        let response = client.request(method::AGENTS_SET_OPTION, params).await;
        assert!(response.error.is_some(), "unadvertised value was accepted");
    }

    // Modes are validated from the same advertised set and update the snapshot.
    let changed_mode = client
        .request(
            method::AGENTS_SET_MODE,
            serde_json::json!({ "session_id": session_id, "mode_id": "plan" }),
        )
        .await;
    assert!(
        changed_mode.error.is_none(),
        "agents.set_mode failed: {:?}",
        changed_mode.error
    );
    assert_eq!(
        changed_mode.result.expect("set_mode result")["modes"]["current_mode_id"],
        "plan"
    );

    let rejected = client
        .request(
            method::AGENTS_SET_MODE,
            serde_json::json!({ "session_id": session_id, "mode_id": "not-a-mode" }),
        )
        .await;
    assert!(
        rejected.error.is_some(),
        "an unadvertised mode was accepted"
    );
}

/// Agent-initiated `config_option_update` and `current_mode_update` notifications
/// replace the session snapshot rather than being ignored.
#[tokio::test]
async fn agent_side_config_and_mode_updates_replace_the_session_snapshot() {
    let update = serde_json::json!([{
        "id": "live",
        "name": "Live option",
        "category": "vendor_custom",
        "type": "select",
        "currentValue": "one",
        "options": [
            { "value": "one", "name": "One" },
            { "value": "two", "name": "Two" }
        ]
    }]);
    let daemon = DaemonHarness::spawn_with(move |_| {
        vec![
            ("FAKE_ACP_CONFIG_UPDATE".to_string(), update.to_string()),
            ("FAKE_ACP_CURRENT_MODE".to_string(), "plan".to_string()),
        ]
    });
    wait_for_unix_socket(daemon.socket(), &daemon).await;
    let mut client = WireClient::connect(daemon.socket()).await;

    let started = client
        .request(
            method::AGENTS_START,
            serde_json::json!({ "agent": "echo", "prompt": "go" }),
        )
        .await;
    assert!(
        started.error.is_none(),
        "agents.start failed: {:?}",
        started.error
    );
    let session_id = started.result.expect("agents.start result")["session"]["id"]
        .as_str()
        .expect("session id")
        .to_string();

    let options = loop {
        let response = client
            .request(
                method::AGENTS_OPTIONS,
                serde_json::json!({ "session_id": session_id }),
            )
            .await;
        assert!(
            response.error.is_none(),
            "agents.options failed: {:?}",
            response.error
        );
        let result = response.result.expect("agents.options result");
        if result["config_options"]
            .as_array()
            .is_some_and(|options| options.len() == 1)
        {
            break result;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(options["config_options"][0]["id"], "live");
    assert_eq!(
        options["config_options"][0]["category"], "vendor_custom",
        "unknown categories are preserved"
    );
    assert_eq!(options["modes"]["current_mode_id"], "plan");
}
