//! End-to-end tests for loading a persisted ACP session.
//!
//! Three fake agents share the same binary but advertise different behaviour:
//! one supports `session/load`, one does not advertise the capability, and one
//! advertises it but fails the request. The daemon must restore only the first
//! and report the other two distinctly, never opening a fresh session.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use base64::Engine;
use favetto_core::rpc::{method, push};
use favetto_tui::client::{Client, Transport};
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(25);

/// An isolated daemon plus its scratch workspace.
struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    listen: String,
    child: Child,
}

impl Daemon {
    fn spawn() -> Self {
        let root = scratch_root("session-load");
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        let work_dir = root.join("work");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(&work_dir).unwrap();

        let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
        let echo_load = root.join("echo-load");
        let echo_prompt = root.join("echo-prompt");
        let bad_load = root.join("bad-load");
        let replay_gate = root.join("replay-gate");
        let replay_bad_gate = root.join("replay-bad-gate");
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"echo\"\n\n\
                 [agents.echo]\ncommand = {agent:?}\nargs = []\n\
                 [agents.echo.env]\n\
                 FAKE_ACP_LOAD_MARKER = {echo_load:?}\n\
                 FAKE_ACP_PROMPT_MARKER = {echo_prompt:?}\n\n\
                 [agents.nope]\ncommand = {agent:?}\nargs = []\n\
                 [agents.nope.env]\nFAKE_ACP_LOAD_SUPPORTED = \"0\"\n\n\
                 [agents.bad]\ncommand = {agent:?}\nargs = []\n\
                 [agents.bad.env]\n\
                 FAKE_ACP_LOAD_RESULT = \"error\"\n\
                 FAKE_ACP_LOAD_MARKER = {bad_load:?}\n\n\
                 [agents.replay]\ncommand = {agent:?}\nargs = []\n\
                 [agents.replay.env]\n\
                 FAKE_ACP_LOAD_GATE_DIR = {replay_gate:?}\n\
                 FAKE_ACP_LOAD_USER = \"restored question\"\n\
                 FAKE_ACP_LOAD_UPDATES = \"history-one|history-two|history-three\"\n\n\
                 [agents.replay-bad]\ncommand = {agent:?}\nargs = []\n\
                 [agents.replay-bad.env]\n\
                 FAKE_ACP_LOAD_RESULT = \"error\"\n\
                 FAKE_ACP_LOAD_GATE_DIR = {replay_bad_gate:?}\n\
                 FAKE_ACP_LOAD_UPDATES = \"partial-one|partial-two\"\n"
            ),
        )
        .unwrap();

        Self::spawn_at(root)
    }

    /// Start a daemon on an already-prepared root (used after a restart).
    fn spawn_at(root: PathBuf) -> Self {
        let socket = root.join("favetto.sock");
        let _ = std::fs::remove_file(&socket);
        let listen = format!("127.0.0.1:{}", free_port());
        let child = spawn_daemon_child(&root, &socket, &listen);
        Self {
            root,
            socket,
            listen,
            child,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// Stop the process without deleting its data, so a new daemon can reopen
    /// the same store and socket.
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn restart(&mut self, config: &str) {
        self.stop();
        std::fs::write(self.config_path(), config).unwrap();
        self.socket = self.root.join("favetto.sock");
        let _ = std::fs::remove_file(&self.socket);
        let listen = self.listen.clone();
        self.child = spawn_daemon_child(&self.root, &self.socket, &listen);
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("data/favetto.db")
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }
}

fn scratch_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("favetto-{tag}-{}-{nanos}", std::process::id()))
}

fn spawn_daemon_child(root: &Path, socket: &Path, listen: &str) -> Child {
    let log = std::fs::File::create(root.join("daemon.log")).unwrap();
    Command::new(env!("CARGO_BIN_EXE_favetto"))
        .arg("daemon")
        .arg("--config")
        .arg(root.join("config.toml"))
        .arg("--data-dir")
        .arg(root.join("data"))
        .arg("--tasks-dir")
        .arg(root.join("tasks"))
        .arg("--socket")
        .arg(socket)
        .arg("--listen")
        .arg(listen)
        .current_dir(root)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .env_remove("FAVETTO_CONFIG")
        .env_remove("FAVETTO_DATA_DIR")
        .spawn()
        .expect("spawn favetto daemon")
}

/// A free loopback port. The listener is dropped immediately, so a small race
/// remains; acceptable for a test.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn wait_for_socket(path: &Path) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!(
                "daemon socket {} never became ready: {error}",
                path.display()
            ),
        }
    }
}

/// Block until the daemon's HTTP/WebSocket listener accepts connections.
async fn wait_for_tcp(listen: &str) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::TcpStream::connect(listen).await {
            Ok(_) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("daemon HTTP listener {listen} never became ready: {error}"),
        }
    }
}

/// The bearer token the daemon wrote to its data directory.
fn daemon_token(daemon: &Daemon) -> String {
    std::fs::read_to_string(daemon.root.join("data/token"))
        .expect("daemon token")
        .trim()
        .to_string()
}

/// Send one MessagePack `Frame::Request` to `POST /rpc` and decode the response.
async fn http_rpc(
    http: &reqwest::Client,
    listen: &str,
    token: &str,
    id: u64,
    method_name: &str,
    params: serde_json::Value,
) -> favetto_core::rpc::Response {
    let body = favetto_core::wire::encode(&favetto_core::rpc::Frame::Request(
        favetto_core::rpc::Request {
            id,
            method: method_name.to_string(),
            params,
        },
    ))
    .expect("encode request frame");
    let response = http
        .post(format!("http://{listen}/rpc"))
        .header("content-type", "application/x-msgpack")
        .bearer_auth(token)
        .body(body)
        .send()
        .await
        .unwrap_or_else(|error| panic!("POST /rpc ({method_name}): {error}"));
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "{method_name}: HTTP status"
    );
    let bytes = response.bytes().await.expect("rpc response body");
    match favetto_core::wire::decode(&bytes).expect("decode response frame") {
        favetto_core::rpc::Frame::Response(resp) => resp,
        other => panic!("{method_name}: expected a response frame, got {other:?}"),
    }
}

async fn open_db(path: &Path) -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .expect("open the daemon database")
}

struct SessionRow {
    external_session_id: Option<String>,
    status: String,
}

async fn session_row(pool: &SqlitePool, id: &str) -> Option<SessionRow> {
    sqlx::query("SELECT external_session_id, status FROM agent_sessions WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|row| SessionRow {
            external_session_id: row.get("external_session_id"),
            status: row.get("status"),
        })
}

async fn wait_for_status(pool: &SqlitePool, id: &str, status: &str) -> SessionRow {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(row) = session_row(pool, id).await {
            if row.status == status {
                return row;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let row = session_row(pool, id).await;
            panic!(
                "session {id} did not reach {status}; last: {:?}",
                row.map(|r| r.status)
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

fn result(resp: favetto_core::rpc::Response, method: &str) -> serde_json::Value {
    assert!(resp.error.is_none(), "{method} failed: {:?}", resp.error);
    resp.result.unwrap_or(serde_json::Value::Null)
}

/// Start an interactive session and wait for its durable ACP id.
async fn start_session(
    client: &Client,
    pool: &SqlitePool,
    daemon: &Daemon,
    agent: &str,
) -> (String, String) {
    let started = result(
        client
            .request(
                method::AGENTS_START,
                json!({ "agent": agent, "cwd": daemon.work() }),
            )
            .await
            .unwrap(),
        method::AGENTS_START,
    );
    let local = started["session"]["id"]
        .as_str()
        .expect("session id")
        .to_string();
    let row = wait_for_status(pool, &local, "ready").await;
    let external = row.external_session_id.expect("ACP id");
    (local, external)
}

async fn close_session(client: &Client, pool: &SqlitePool, local: &str) {
    result(
        client
            .request(method::AGENTS_CLOSE, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_CLOSE,
    );
    wait_for_status(pool, local, "ended").await;
}

/// A one-agent config with the fake ACP server. `env` is a TOML table body.
fn echo_config(env: &str) -> String {
    let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
    format!(
        "[agent]\ndefault = \"echo\"\n\n\
         [agents.echo]\ncommand = {agent:?}\nargs = []\n\
         [agents.echo.env]\n{env}"
    )
}

/// Wait until an attached transcript contains `needle`.
async fn wait_for_attach_text(client: &Client, local: &str, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        let (_, text, _) = attach(client, local).await;
        if text.contains(needle) {
            return text;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("transcript never contained {needle:?}; last: {text:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The persisted task status and attempt.
async fn task_status(pool: &SqlitePool, id: &str) -> Option<(String, i64)> {
    sqlx::query("SELECT status, attempt FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|row| (row.get("status"), row.get("attempt")))
}

async fn wait_for_task_status(pool: &SqlitePool, id: &str, status: &str) -> (String, i64) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(row) = task_status(pool, id).await {
            if row.0 == status {
                return row;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "task {id} did not reach {status}; last: {:?}",
                task_status(pool, id).await
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The run statuses for one task, newest first.
async fn run_statuses(pool: &SqlitePool, task_id: &str) -> Vec<String> {
    sqlx::query("SELECT status FROM task_runs WHERE task_id = ? ORDER BY attempt DESC")
        .bind(task_id)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get("status"))
        .collect()
}

/// Wait until the fake agent has announced that a gated load is holding its
/// replay. File-based, so the test never races a wall-clock delay.
async fn wait_for_file(path: &Path) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if path.exists() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{} was never created", path.display());
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The attach snapshot's plain-text transcript.
fn attach_text(value: &serde_json::Value) -> String {
    let encoded = value["data"].as_str().expect("attach data");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("base64 attach data");
    String::from_utf8(bytes).expect("utf-8 transcript")
}

/// Attach to `local` and return the decoded transcript plus the cursor.
async fn attach(client: &Client, local: &str) -> (serde_json::Value, String, u64) {
    let value = result(
        client
            .request(method::AGENTS_ATTACH, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_ATTACH,
    );
    let seq = value["seq"].as_u64().unwrap_or(0);
    let text = attach_text(&value);
    (value, text, seq)
}

/// Collect this session's `agent.event` pushes observed by a subscriber.
///
/// Waits for a short quiet period; readiness is published only after replay is
/// in the feed, so everything the load produced is already on the bus.
async fn agent_events(
    rx: &mut tokio::sync::broadcast::Receiver<favetto_core::rpc::Notification>,
    local: &str,
) -> Vec<(u64, serde_json::Value)> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Ok(notification)) if notification.method == push::AGENT_EVENT => {
                let params = &notification.params;
                if params["session_id"].as_str() != Some(local) {
                    continue;
                }
                let Some(seq) = params["seq"].as_u64() else {
                    continue;
                };
                out.push((seq, params["event"].clone()));
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => break,
        }
    }
    out
}

/// Every assistant/user text delta, in delivery order.
fn text_deltas(events: &[(u64, serde_json::Value)]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|(_, event)| {
            if event["event"].as_str() != Some("text_delta") {
                return None;
            }
            Some((
                event["role"].as_str().unwrap_or_default().to_string(),
                event["text"].as_str().unwrap_or_default().to_string(),
            ))
        })
        .collect()
}

#[tokio::test]
async fn load_replays_history_in_order_before_it_is_ready() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");
    let observer = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect the observer");
    let mut pushes = observer.subscribe();
    let pool = open_db(&daemon.db_path()).await;

    let (local, _external) = start_session(&client, &pool, &daemon, "replay").await;
    close_session(&client, &pool, &local).await;

    let load = {
        let client = client.clone();
        let local = local.clone();
        tokio::spawn(async move {
            client
                .request(method::AGENTS_LOAD, json!({ "session_id": local }))
                .await
                .unwrap()
        })
    };

    // The fake agent has emitted its whole replay but is holding the response.
    wait_for_file(&daemon.root.join("replay-gate/load-ready")).await;

    // Discard pushes from the just-closed creation session; the replay itself
    // has not been published yet.
    while pushes.try_recv().is_ok() {}

    // Attaching during the load sees an explicitly loading session with no
    // transcript yet; the replay is not allowed to leak early.
    let (during, during_text, during_seq) = attach(&observer, &local).await;
    assert_eq!(
        during["session"]["load_state"], "loading",
        "an in-flight restore is explicitly loading"
    );
    assert_eq!(during_text, "", "no replay is exposed before it succeeds");
    assert_eq!(during_seq, 0, "no replay events have a sequence yet");

    std::fs::write(daemon.root.join("replay-gate/load-release"), b"").unwrap();
    let loaded = result(load.await.expect("load task"), method::AGENTS_LOAD);
    assert_eq!(loaded["session"]["load_state"], "ready");
    assert!(
        loaded["session"].get("load_error").is_none(),
        "a successful load carries no error"
    );
    assert_eq!(
        attach_text(&loaded),
        "history-onehistory-twohistory-three",
        "the complete replay is attached, in arrival order"
    );
    let loaded_seq = loaded["seq"].as_u64().unwrap();
    assert!(
        loaded_seq >= 3,
        "the replay has sequence numbers before readiness: {loaded_seq}"
    );

    // Reconnecting after readiness returns the same single copy of history; the
    // replay is not re-emitted per attach.
    let (again, again_text, again_seq) = attach(&observer, &local).await;
    assert_eq!(again["session"]["load_state"], "ready");
    assert_eq!(again_text, "history-onehistory-twohistory-three");
    assert_eq!(again_seq, loaded_seq);
    let (_, third_text, third_seq) = attach(&observer, &local).await;
    assert_eq!(third_text, "history-onehistory-twohistory-three");
    assert_eq!(third_seq, loaded_seq);

    // The event feed also contains the replay exactly once, in order, with
    // strictly increasing cursors.
    let events = agent_events(&mut pushes, &local).await;
    let deltas = text_deltas(&events);
    assert_eq!(
        deltas,
        vec![
            ("user".to_string(), "restored question".to_string()),
            ("assistant".to_string(), "history-one".to_string()),
            ("assistant".to_string(), "history-two".to_string()),
            ("assistant".to_string(), "history-three".to_string()),
        ]
    );
    let text_seqs: Vec<u64> = events
        .iter()
        .filter(|(_, event)| event["event"].as_str() == Some("text_delta"))
        .map(|(seq, _)| *seq)
        .collect();
    assert!(
        text_seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "replay event sequence must be strictly increasing: {text_seqs:?}"
    );

    close_session(&client, &pool, &local).await;
}

#[tokio::test]
async fn failed_load_discards_its_partial_replay() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");
    let observer = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect the observer");
    let mut pushes = observer.subscribe();
    let pool = open_db(&daemon.db_path()).await;

    let (local, _external) = start_session(&client, &pool, &daemon, "replay-bad").await;
    close_session(&client, &pool, &local).await;

    let load = {
        let client = client.clone();
        let local = local.clone();
        tokio::spawn(async move {
            client
                .request(method::AGENTS_LOAD, json!({ "session_id": local }))
                .await
                .unwrap()
        })
    };

    // The failed load emits its replay updates first, then holds the response.
    wait_for_file(&daemon.root.join("replay-bad-gate/load-ready")).await;
    while pushes.try_recv().is_ok() {}
    let (during, during_text, during_seq) = attach(&observer, &local).await;
    assert_eq!(during["session"]["load_state"], "loading");
    assert_eq!(during_text, "");
    assert_eq!(during_seq, 0);

    std::fs::write(daemon.root.join("replay-bad-gate/load-release"), b"").unwrap();
    let response = load.await.expect("load task");
    let error = response
        .error
        .expect("a failed load reports an error")
        .message;
    assert!(error.contains("could not load ACP session"), "{error}");

    let (failed, failed_text, _failed_seq) = attach(&observer, &local).await;
    assert_eq!(failed["session"]["load_state"], "failed");
    assert!(
        failed["session"]["load_error"]
            .as_str()
            .is_some_and(|message| message.contains("could not load ACP session")),
        "the snapshot carries the restore failure"
    );
    assert_eq!(
        failed_text, "",
        "a failed load must not expose its partial replay as restored text"
    );

    // Nothing from the discarded replay reached the live feed either.
    let events = agent_events(&mut pushes, &local).await;
    let deltas = text_deltas(&events);
    assert!(
        deltas.iter().all(|(_, text)| !text.contains("partial")),
        "partial replay leaked into the feed: {deltas:?}"
    );
}

#[tokio::test]
async fn persisted_sessions_load_only_when_the_agent_supports_it() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");
    let pool = open_db(&daemon.db_path()).await;

    // 1. Supported: the stored local id and ACP id survive a load, the stored
    //    cwd is used even when the request carries a different one, and no
    //    prompt is submitted as part of loading.
    let (local, external) = start_session(&client, &pool, &daemon, "echo").await;
    close_session(&client, &pool, &local).await;

    let history = result(
        client
            .request(method::AGENTS_HISTORY, json!({ "limit": 10 }))
            .await
            .unwrap(),
        method::AGENTS_HISTORY,
    );
    let rows = history.as_array().expect("history rows");
    let row = rows
        .iter()
        .find(|row| row["id"] == json!(local))
        .expect("the closed session is discoverable");
    assert_eq!(row["agent"], "echo");
    assert_eq!(row["state"], "ended");
    assert_eq!(row["loadable"], true);
    assert_eq!(row["cwd"], json!(daemon.work()));
    assert!(
        !history.to_string().contains(&external),
        "history must not expose the provider ACP id: {history}"
    );

    let loaded = result(
        client
            .request(method::AGENTS_LOAD, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_LOAD,
    );
    assert_eq!(
        loaded["session"]["id"], local,
        "the local id is the routing key"
    );
    assert_eq!(
        loaded["session"]["session_id"], external,
        "the provider ACP id stays separate metadata"
    );
    assert_eq!(
        std::fs::read_to_string(daemon.root.join("echo-load")).unwrap(),
        format!("{external}\n{}\n", daemon.work().display()),
        "session/load must carry the stored ACP id and working directory"
    );
    assert!(
        !daemon.root.join("echo-prompt").exists(),
        "loading a conversation must not send a prompt"
    );
    wait_for_status(&pool, &local, "ready").await;
    close_session(&client, &pool, &local).await;

    // 2. Unsupported: no silent fallback to a new session.
    let (local, external) = start_session(&client, &pool, &daemon, "nope").await;
    close_session(&client, &pool, &local).await;
    let error = client
        .request(
            method::AGENTS_START,
            json!({ "agent": "nope", "session_id": local }),
        )
        .await
        .unwrap();
    let message = error.error.expect("unsupported load error").message;
    assert!(
        message.contains("does not support loading ACP sessions"),
        "{message}"
    );
    let failed = wait_for_status(&pool, &local, "failed").await;
    assert_eq!(
        failed.external_session_id.as_deref(),
        Some(external.as_str()),
        "the unsupported load keeps the known identity"
    );

    // 3. Agent-reported failure: the request reached the agent and failed, so
    //    it is distinguishable from both success and unsupported capability.
    let (local, external) = start_session(&client, &pool, &daemon, "bad").await;
    close_session(&client, &pool, &local).await;
    let error = client
        .request(
            method::AGENTS_START,
            json!({ "agent": "bad", "session_id": local }),
        )
        .await
        .unwrap();
    let message = error.error.expect("failed load error").message;
    assert!(message.contains("could not load ACP session"), "{message}");
    assert!(
        std::fs::read_to_string(daemon.root.join("bad-load"))
            .is_ok_and(|marker| marker.starts_with(&external)),
        "the agent must have received the load request"
    );
    wait_for_status(&pool, &local, "failed").await;

    assert!(session_row(&pool, &local).await.is_some());
}

/// Regression for the GPUI task-conversation restore: the connection-scoped
/// methods were intercepted only by the socket/WebSocket `handle_request`, so
/// over HTTP `POST /rpc` they fell through to `unknown method: agents.load`.
/// They must dispatch through the shared helpers with no connection to
/// subscribe.
#[tokio::test]
async fn http_rpc_restores_a_closed_task_conversation() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    wait_for_tcp(&daemon.listen).await;
    let http = reqwest::Client::new();
    let token = daemon_token(&daemon);
    let pool = open_db(&daemon.db_path()).await;

    // `tasks.start_oneshot` opens an inline interactive session from one HTTP
    // request; closing it over HTTP must work too.
    let oneshot = result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            1,
            method::TASKS_START_ONESHOT,
            json!({ "agent": "echo", "cwd": daemon.work() }),
        )
        .await,
        method::TASKS_START_ONESHOT,
    );
    let oneshot_local = oneshot["session"]["id"]
        .as_str()
        .expect("one-shot session id")
        .to_string();
    wait_for_status(&pool, &oneshot_local, "ready").await;
    result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            2,
            method::AGENTS_CLOSE,
            json!({ "session_id": oneshot_local }),
        )
        .await,
        method::AGENTS_CLOSE,
    );
    wait_for_status(&pool, &oneshot_local, "ended").await;

    // `agents.start` returns the attached snapshot with no connection scope.
    let started = result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            3,
            method::AGENTS_START,
            json!({ "agent": "echo", "cwd": daemon.work() }),
        )
        .await,
        method::AGENTS_START,
    );
    let local = started["session"]["id"]
        .as_str()
        .expect("session id")
        .to_string();
    assert!(
        started["data"].is_string(),
        "agents.start must include the attach snapshot"
    );
    let row = wait_for_status(&pool, &local, "ready").await;
    let external = row.external_session_id.expect("ACP id");

    // `agents.attach` is dispatched directly over HTTP as well.
    let attached = result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            4,
            method::AGENTS_ATTACH,
            json!({ "session_id": local }),
        )
        .await,
        method::AGENTS_ATTACH,
    );
    assert_eq!(attached["session"]["id"], json!(local));

    result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            5,
            method::AGENTS_CLOSE,
            json!({ "session_id": local }),
        )
        .await,
        method::AGENTS_CLOSE,
    );
    wait_for_status(&pool, &local, "ended").await;

    // The reported regression: restore the closed conversation through HTTP.
    let loaded = result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            6,
            method::AGENTS_LOAD,
            json!({ "session_id": local }),
        )
        .await,
        method::AGENTS_LOAD,
    );
    assert_eq!(loaded["session"]["id"], json!(local));
    assert_eq!(loaded["session"]["session_id"], json!(external));
    assert_eq!(loaded["session"]["load_state"], "ready");
    assert_eq!(
        std::fs::read_to_string(daemon.root.join("echo-load")).unwrap(),
        format!("{external}\n{}\n", daemon.work().display()),
        "the stored ACP id and working directory are used"
    );
    assert!(
        !daemon.root.join("echo-prompt").exists(),
        "restoring a conversation must not send a prompt"
    );

    // `events.subscribe` has no replay to stream on a one-shot response, but
    // acknowledges the subscription instead of being unknown.
    let subscribed = result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            7,
            method::EVENTS_SUBSCRIBE,
            json!({}),
        )
        .await,
        method::EVENTS_SUBSCRIBE,
    );
    assert_eq!(subscribed["subscribed"], json!(true));

    result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            8,
            method::AGENTS_CLOSE,
            json!({ "session_id": local }),
        )
        .await,
        method::AGENTS_CLOSE,
    );
}

#[tokio::test]
async fn a_failed_initialization_reports_a_failed_restore_without_a_new_session() {
    let mut daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");
    let pool = open_db(&daemon.db_path()).await;

    let (local, external) = start_session(&client, &pool, &daemon, "echo").await;
    close_session(&client, &pool, &local).await;
    drop(client);

    daemon.restart(&echo_config("FAKE_ACP_INIT_ERROR = \"1\"\n"));
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the restarted daemon");
    let pool = open_db(&daemon.db_path()).await;

    let response = client
        .request(method::AGENTS_LOAD, json!({ "session_id": local }))
        .await
        .unwrap();
    let message = response
        .error
        .expect("a failed initialize fails the load")
        .message;
    assert!(message.contains("initialize"), "{message}");

    let (failed, text, _seq) = attach(&client, &local).await;
    assert_eq!(failed["session"]["load_state"], "failed");
    assert!(
        failed["session"]["load_error"]
            .as_str()
            .is_some_and(|error| error.contains("initialize")),
        "the snapshot carries the initialization failure"
    );
    assert_eq!(text, "", "a failed initialization exposes no replay");

    let row = session_row(&pool, &local).await.expect("durable record");
    assert_eq!(row.status, "failed");
    assert_eq!(
        row.external_session_id.as_deref(),
        Some(external.as_str()),
        "no fresh ACP session replaced the stored identity"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "the load did not create another session record");
}

#[tokio::test]
async fn restarted_daemon_restores_replay_before_readiness_and_accepts_a_follow_up() {
    let mut daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");
    let pool = open_db(&daemon.db_path()).await;

    let (local, external) = start_session(&client, &pool, &daemon, "echo").await;
    close_session(&client, &pool, &local).await;
    drop(client);

    // Reopen the same store with the replay gate and a prompt marker. The ACP
    // agent persists the conversation under `external`, which only the daemon
    // knows; the client still routes the load by `local`.
    let load_marker = daemon.root.join("restart-load");
    let prompt_marker = daemon.root.join("restart-prompt");
    let gate = daemon.root.join("restart-replay-gate");
    daemon.restart(&echo_config(&format!(
        "FAKE_ACP_LOAD_MARKER = {load_marker:?}\n\
         FAKE_ACP_PROMPT_MARKER = {prompt_marker:?}\n\
         FAKE_ACP_LOAD_GATE_DIR = {gate:?}\n\
         FAKE_ACP_LOAD_USER = \"restored question\"\n\
         FAKE_ACP_LOAD_UPDATES = \"history-one|history-two|history-three\"\n"
    )));
    wait_for_socket(&daemon.socket).await;

    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the restarted daemon");
    let observer = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect the observer");
    let pool = open_db(&daemon.db_path()).await;

    let history = result(
        client
            .request(method::AGENTS_HISTORY, json!({ "limit": 10 }))
            .await
            .unwrap(),
        method::AGENTS_HISTORY,
    );
    let row = history
        .as_array()
        .expect("history rows")
        .iter()
        .find(|row| row["id"] == json!(local))
        .expect("the session survives the restart");
    assert_eq!(row["state"], "ended");
    assert_eq!(row["loadable"], true);
    assert!(
        !history.to_string().contains(&external),
        "history must not expose the provider ACP id: {history}"
    );

    let load = {
        let client = client.clone();
        let local = local.clone();
        tokio::spawn(async move {
            client
                .request(method::AGENTS_LOAD, json!({ "session_id": local }))
                .await
                .unwrap()
        })
    };
    wait_for_file(&gate.join("load-ready")).await;

    let (during, during_text, during_seq) = attach(&observer, &local).await;
    assert_eq!(during["session"]["load_state"], "loading");
    assert_eq!(
        during_text, "",
        "a restarted daemon exposes no replay before the load succeeds"
    );
    assert_eq!(during_seq, 0);

    std::fs::write(gate.join("load-release"), b"").unwrap();
    let loaded = result(load.await.expect("load task"), method::AGENTS_LOAD);
    assert_eq!(loaded["session"]["id"], local);
    assert_eq!(
        loaded["session"]["session_id"], external,
        "the stored provider ACP id is still the one passed to session/load"
    );
    assert_eq!(loaded["session"]["load_state"], "ready");
    assert_eq!(
        attach_text(&loaded),
        "history-onehistory-twohistory-three",
        "the replay arrives before readiness"
    );
    assert_eq!(
        std::fs::read_to_string(&load_marker).unwrap(),
        format!("{external}\n{}\n", daemon.work().display()),
        "the stored ACP id and directory survive store recreation"
    );
    assert!(!prompt_marker.exists(), "a load must not submit a prompt");

    let prompted = result(
        client
            .request(
                method::AGENTS_PROMPT,
                json!({ "session_id": local, "prompt": "follow-up after restart" }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROMPT,
    );
    assert_eq!(prompted["accepted"], true);
    wait_for_file(&prompt_marker).await;
    assert_eq!(
        std::fs::read_to_string(&prompt_marker).unwrap(),
        "follow-up after restart"
    );
    let transcript =
        wait_for_attach_text(&client, &local, "echo-agent: follow-up after restart").await;
    assert!(
        transcript.contains("history-onehistory-twohistory-three"),
        "the replayed history is kept alongside the follow-up: {transcript:?}"
    );

    close_session(&client, &pool, &local).await;
}

#[tokio::test]
async fn restart_during_an_active_task_never_resumes_or_succeeds_it() {
    let mut daemon = Daemon::spawn();
    let tasks_dir = daemon.root.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("echo_task.md"),
        "agent = \"echo\"\n---\necho: restart\n",
    )
    .unwrap();
    // The initial daemon gates the prompt turn, so the task is genuinely
    // in-flight when the daemon is killed.
    daemon.restart(&echo_config(&format!(
        "FAKE_ACP_GATE_DIR = {:?}\nFAKE_ACP_TITLE = \"Interrupted session\"\n",
        daemon.root
    )));
    wait_for_socket(&daemon.socket).await;

    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");
    let pool = open_db(&daemon.db_path()).await;

    let started = result(
        client
            .request(method::TASKS_START, json!({ "name": "echo_task" }))
            .await
            .unwrap(),
        method::TASKS_START,
    );
    let task_id = started["id"].as_str().expect("task id").to_string();
    wait_for_file(&daemon.root.join("agent-ready")).await;

    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    let session_id = loop {
        let row = sqlx::query(
            "SELECT id FROM agent_sessions WHERE task_id = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&task_id)
        .fetch_optional(&pool)
        .await
        .unwrap();
        if let Some(row) = row {
            break row.get::<String, _>("id");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no session row for task {task_id}"
        );
        tokio::time::sleep(POLL).await;
    };
    let session = wait_for_status(&pool, &session_id, "ready").await;
    let external = session.external_session_id.expect("ACP id");
    let before_task = task_status(&pool, &task_id).await.expect("task row");
    assert_eq!(before_task.0, "running");
    assert_eq!(run_statuses(&pool, &task_id).await, vec!["running"]);
    drop(client);

    let prompt_marker = daemon.root.join("restart-task-prompt");
    daemon.restart(&echo_config(&format!(
        "FAKE_ACP_LOAD_MARKER = {:?}\n\
         FAKE_ACP_PROMPT_MARKER = {prompt_marker:?}\n\
         FAKE_ACP_LOAD_USER = \"interrupted question\"\n\
         FAKE_ACP_LOAD_UPDATES = \"interrupted history\"\n",
        daemon.root.join("restart-task-load")
    )));
    wait_for_socket(&daemon.socket).await;

    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the restarted daemon");
    let pool = open_db(&daemon.db_path()).await;

    // Startup reconciliation interrupts the run and fails the stale task; the
    // durable session record stays loadable.
    let after_task = wait_for_task_status(&pool, &task_id, "failed").await;
    assert_eq!(after_task.0, "failed");
    assert_eq!(
        after_task.1, before_task.1,
        "no retry attempt was scheduled"
    );
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    while run_statuses(&pool, &task_id).await != vec!["interrupted"] {
        assert!(
            tokio::time::Instant::now() < deadline,
            "run was not marked interrupted: {:?}",
            run_statuses(&pool, &task_id).await
        );
        tokio::time::sleep(POLL).await;
    }

    let history = result(
        client
            .request(method::AGENTS_HISTORY, json!({ "limit": 10 }))
            .await
            .unwrap(),
        method::AGENTS_HISTORY,
    );
    let row = history
        .as_array()
        .expect("history rows")
        .iter()
        .find(|row| row["id"] == json!(session_id))
        .expect("the interrupted session is listed");
    assert_eq!(row["state"], "ended");
    assert_eq!(row["loadable"], true);
    assert_eq!(row["task_id"], task_id);

    let loaded = result(
        client
            .request(method::AGENTS_LOAD, json!({ "session_id": session_id }))
            .await
            .unwrap(),
        method::AGENTS_LOAD,
    );
    assert_eq!(loaded["session"]["id"], session_id);
    assert_eq!(loaded["session"]["session_id"], external);
    assert_eq!(loaded["session"]["load_state"], "ready");
    assert_eq!(attach_text(&loaded), "interrupted history");
    assert!(
        !prompt_marker.exists(),
        "restoring the conversation must not run the task's prompt"
    );

    // The restored session is usable, but it never resumes or completes the
    // interrupted task.
    let prompted = result(
        client
            .request(
                method::AGENTS_PROMPT,
                json!({ "session_id": session_id, "prompt": "what happened?" }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROMPT,
    );
    assert_eq!(prompted["accepted"], true);
    wait_for_file(&prompt_marker).await;
    wait_for_attach_text(&client, &session_id, "echo-agent: what happened?").await;

    assert_eq!(
        task_status(&pool, &task_id).await,
        Some(after_task.clone()),
        "loading a past session neither re-runs nor succeeds the task"
    );
    assert_eq!(
        run_statuses(&pool, &task_id).await,
        vec!["interrupted"],
        "no new task run was created"
    );
    let task = result(
        client
            .request(method::TASKS_GET, json!({ "id": task_id }))
            .await
            .unwrap(),
        method::TASKS_GET,
    );
    assert_eq!(task["status"], "failed");

    close_session(&client, &pool, &session_id).await;
}
