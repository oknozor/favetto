//! End-to-end tests for provider-owned ACP session history.
//!
//! The fake agents advertise different optional session capabilities:
//!
//! - `echo` supports `session/list`, `session/load` and `session/delete`;
//! - `resume-only` supports `session/list` and `session/resume` but not load;
//! - `nope` advertises neither list nor delete;
//! - `bad-delete` advertises delete but fails the request.
//!
//! Provider history must stay distinct from `agents.history`, be capability
//! gated, paginate by cursor, filter by cwd, and import a local record only on
//! resume. Deletes remove the provider transcript without touching local
//! metadata.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::rpc::method;
use favetto_tui::client::{Client, Transport};
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(25);

struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    listen: String,
    child: Child,
}

impl Daemon {
    fn spawn() -> Self {
        let root = scratch_root("provider-sessions");
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        let work_dir = root.join("work");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(&work_dir).unwrap();

        let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"echo\"\n\n\
                 [agents.echo]\ncommand = {agent:?}\nargs = []\n\
                 [agents.echo.env]\n\
                 FAKE_ACP_LIST_SUPPORTED = \"1\"\n\
                 FAKE_ACP_DELETE_SUPPORTED = \"1\"\n\
                 FAKE_ACP_LIST_COUNT = \"3\"\n\
                 FAKE_ACP_LIST_PAGE_SIZE = \"2\"\n\
                 FAKE_ACP_PROVIDER_CWD = {work:?}\n\
                 FAKE_ACP_DELETE_MARKER = {marker:?}\n\
                 FAKE_ACP_LOAD_USER = \"provider question\"\n\
                 FAKE_ACP_LOAD_UPDATES = \"provider one|provider two\"\n\n\
                 [agents.resume-only]\ncommand = {agent:?}\nargs = []\n\
                 [agents.resume-only.env]\n\
                 FAKE_ACP_LIST_SUPPORTED = \"1\"\n\
                 FAKE_ACP_RESUME_SUPPORTED = \"1\"\n\
                 FAKE_ACP_LOAD_SUPPORTED = \"0\"\n\
                 FAKE_ACP_LIST_COUNT = \"1\"\n\
                 FAKE_ACP_PROVIDER_CWD = {work:?}\n\n\
                 [agents.nope]\ncommand = {agent:?}\nargs = []\n\
                 [agents.nope.env]\n\
                 FAKE_ACP_LOAD_SUPPORTED = \"0\"\n\
                 FAKE_ACP_PROVIDER_CWD = {work:?}\n\n\
                 [agents.bad-delete]\ncommand = {agent:?}\nargs = []\n\
                 [agents.bad-delete.env]\n\
                 FAKE_ACP_LIST_SUPPORTED = \"1\"\n\
                 FAKE_ACP_DELETE_SUPPORTED = \"1\"\n\
                 FAKE_ACP_DELETE_RESULT = \"error\"\n\
                 FAKE_ACP_LIST_COUNT = \"1\"\n\
                 FAKE_ACP_PROVIDER_CWD = {work:?}\n",
                work = work_dir,
                marker = root.join("deleted"),
            ),
        )
        .unwrap();

        let socket = root.join("favetto.sock");
        let listen = format!("127.0.0.1:{}", free_port());
        let child = spawn_daemon_child(&root, &socket, &listen);
        Self {
            root,
            socket,
            listen,
            child,
        }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("data/favetto.db")
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
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

async fn wait_for_socket(path: &Path) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("daemon socket never became ready: {error}"),
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

fn result(response: favetto_core::rpc::Response, method: &str) -> serde_json::Value {
    assert!(
        response.error.is_none(),
        "{method} failed: {:?}",
        response.error
    );
    response.result.unwrap_or(serde_json::Value::Null)
}

fn attach_text(value: &serde_json::Value) -> String {
    let encoded = value["data"].as_str().unwrap_or_default();
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        .unwrap_or_default();
    String::from_utf8(bytes).unwrap_or_default()
}

async fn connect(daemon: &Daemon) -> Client {
    Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon")
}

/// Provider history opens a short-lived ACP connection, so under a fully
/// parallel suite the client's default 5s request timeout is too tight.
trait SlowRequest {
    async fn slow_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<favetto_core::rpc::Response>;
}

impl SlowRequest for Client {
    async fn slow_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<favetto_core::rpc::Response> {
        self.request_with_timeout(method, params, Duration::from_secs(30))
            .await
    }
}

async fn provider_sessions(client: &Client, params: serde_json::Value) -> Vec<serde_json::Value> {
    result(
        client
            .slow_request(method::AGENTS_PROVIDER_SESSIONS, params)
            .await
            .unwrap(),
        method::AGENTS_PROVIDER_SESSIONS,
    )
    .as_array()
    .expect("provider sessions result is an array")
    .clone()
}

async fn session_row(pool: &SqlitePool, id: &str) -> Option<(String, String, Option<String>)> {
    sqlx::query("SELECT id, agent, external_session_id FROM agent_sessions WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|row| {
            (
                row.get("id"),
                row.get("agent"),
                row.get("external_session_id"),
            )
        })
}

#[tokio::test]
async fn provider_history_is_capability_gated_paginated_and_cwd_filtered() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = connect(&daemon).await;

    // `echo` advertises list; the fake pages two sessions at a time.
    let first = provider_sessions(&client, json!({ "agent": "echo" })).await;
    let echo = &first[0];
    assert_eq!(echo["agent"], "echo");
    assert_eq!(echo["supported"], true);
    assert_eq!(echo["can_resume"], true);
    assert_eq!(echo["can_delete"], true);
    let page = echo["sessions"].as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["session_id"], "provider-ses-0");
    assert_eq!(page[1]["session_id"], "provider-ses-1");
    assert_eq!(page[0]["cwd"], daemon.work().to_string_lossy().as_ref());
    let cursor = echo["next_cursor"].as_str().expect("cursor").to_string();

    let second = provider_sessions(
        &client,
        json!({ "agent": "echo", "cursor": cursor.clone() }),
    )
    .await;
    let echo = &second[0];
    let page = echo["sessions"].as_array().unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0]["session_id"], "provider-ses-2");
    assert!(echo.get("next_cursor").is_none() || echo["next_cursor"].is_null());

    // A cwd filter is passed to the provider, not applied to a local table.
    let filtered =
        provider_sessions(&client, json!({ "agent": "echo", "cwd": daemon.work() })).await;
    assert_eq!(filtered[0]["sessions"].as_array().unwrap().len(), 2);
    let none = provider_sessions(
        &client,
        json!({ "agent": "echo", "cwd": "/definitely/not/this/repo" }),
    )
    .await;
    assert!(none[0]["sessions"].as_array().unwrap().is_empty());

    // An agent without the capability is never sent session/list.
    let unsupported = provider_sessions(&client, json!({ "agent": "nope" })).await;
    let nope = &unsupported[0];
    assert_eq!(nope["supported"], false);
    let error = nope["error"].as_str().expect("unsupported reason");
    assert!(error.contains("does not support"), "{error}");

    // Listing every agent keeps their results and capabilities separate.
    let all = provider_sessions(&client, json!({})).await;
    assert!(all.len() >= 3, "one result per available agent: {all:?}");
    assert!(all.iter().any(|result| result["agent"] == "echo"));
    assert!(all.iter().any(|result| result["agent"] == "resume-only"));

    // A cursor cannot be applied to an aggregate listing.
    let response = client
        .slow_request(method::AGENTS_PROVIDER_SESSIONS, json!({ "cursor": "0" }))
        .await
        .unwrap();
    assert!(response.error.is_some());
}

#[tokio::test]
async fn provider_resume_imports_a_local_record_and_reuses_it() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = connect(&daemon).await;
    let pool = open_db(&daemon.db_path()).await;

    let resumed = result(
        client
            .slow_request(
                method::AGENTS_PROVIDER_RESUME,
                json!({
                    "agent": "echo",
                    "provider_session_id": "provider-ses-0",
                    "cwd": daemon.work(),
                }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROVIDER_RESUME,
    );
    let local = resumed["session"]["id"].as_str().expect("local id");
    assert_ne!(
        local, "provider-ses-0",
        "provider id is not the routing key"
    );
    assert_eq!(resumed["session"]["session_id"], "provider-ses-0");
    assert_eq!(resumed["session"]["load_state"], "ready");
    assert_eq!(resumed["session"]["agent"], "echo");
    assert_eq!(
        attach_text(&resumed),
        "provider oneprovider two",
        "provider resume prefers session/load and applies its replay in order"
    );

    let row = session_row(&pool, local)
        .await
        .expect("the provider session was imported");
    assert_eq!(row.1, "echo");
    assert_eq!(row.2.as_deref(), Some("provider-ses-0"));

    // A second resume reuses the imported row rather than duplicating it.
    let again = result(
        client
            .slow_request(
                method::AGENTS_PROVIDER_RESUME,
                json!({ "agent": "echo", "provider_session_id": "provider-ses-0" }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROVIDER_RESUME,
    );
    assert_eq!(again["session"]["id"], local);
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM agent_sessions")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(count, 1, "resume imports exactly one local record");

    // The provider id remains rejected by the local routing API.
    let rejected = client
        .slow_request(
            method::AGENTS_LOAD,
            json!({ "provider_session_id": "provider-ses-0" }),
        )
        .await
        .unwrap();
    assert!(rejected.error.is_some());

    result(
        client
            .slow_request(method::AGENTS_CLOSE, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_CLOSE,
    );
}

/// `agents.provider_resume` used to be connection-scoped only, so HTTP
/// `POST /rpc` (the GPUI transport) reported `unknown method`. It must import
/// and attach through the shared helper.
#[tokio::test]
async fn http_rpc_can_resume_a_provider_session() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    wait_for_tcp(&daemon.listen).await;
    let http = reqwest::Client::new();
    let token = daemon_token(&daemon);
    let pool = open_db(&daemon.db_path()).await;

    let resumed = result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            1,
            method::AGENTS_PROVIDER_RESUME,
            json!({
                "agent": "echo",
                "provider_session_id": "provider-ses-0",
                "cwd": daemon.work(),
            }),
        )
        .await,
        method::AGENTS_PROVIDER_RESUME,
    );
    let local = resumed["session"]["id"]
        .as_str()
        .expect("local session id")
        .to_string();
    assert_ne!(local, "provider-ses-0", "the provider id is not the key");
    assert_eq!(resumed["session"]["session_id"], "provider-ses-0");
    assert_eq!(resumed["session"]["load_state"], "ready");
    assert_eq!(
        attach_text(&resumed),
        "provider oneprovider two",
        "the HTTP resume attaches the provider replay"
    );

    let row = session_row(&pool, &local)
        .await
        .expect("the provider session was imported");
    assert_eq!(row.1, "echo");
    assert_eq!(row.2.as_deref(), Some("provider-ses-0"));

    result(
        http_rpc(
            &http,
            &daemon.listen,
            &token,
            2,
            method::AGENTS_CLOSE,
            json!({ "session_id": local }),
        )
        .await,
        method::AGENTS_CLOSE,
    );
}

#[tokio::test]
async fn provider_resume_falls_back_to_session_resume_without_replay() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = connect(&daemon).await;

    let resumed = result(
        client
            .slow_request(
                method::AGENTS_PROVIDER_RESUME,
                json!({
                    "agent": "resume-only",
                    "provider_session_id": "provider-ses-0",
                    "cwd": daemon.work(),
                }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROVIDER_RESUME,
    );
    assert_eq!(resumed["session"]["load_state"], "ready");
    assert_eq!(resumed["session"]["session_id"], "provider-ses-0");
    // `session/resume` does not replay history; the transcript is empty rather
    // than fabricated from a load path the agent never ran.
    assert_eq!(resumed["data"].as_str().unwrap_or_default(), "");

    let local = resumed["session"]["id"].as_str().unwrap();
    let prompt = result(
        client
            .slow_request(
                method::AGENTS_PROMPT,
                json!({ "session_id": local, "prompt": "after-resume" }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROMPT,
    );
    assert_eq!(prompt["accepted"], true);

    // The resumed conversation accepts follow-up turns like any local session.
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        let attached = result(
            client
                .slow_request(method::AGENTS_ATTACH, json!({ "session_id": local }))
                .await
                .unwrap(),
            method::AGENTS_ATTACH,
        );
        let text = attach_text(&attached);
        if text.contains("after-resume") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "follow-up prompt never reached the resumed session"
        );
        tokio::time::sleep(POLL).await;
    }

    result(
        client
            .slow_request(method::AGENTS_CLOSE, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_CLOSE,
    );
}

#[tokio::test]
async fn provider_delete_is_capability_gated_and_keeps_local_metadata() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = connect(&daemon).await;

    // `nope` never advertised delete.
    let unsupported = client
        .slow_request(
            method::AGENTS_PROVIDER_DELETE,
            json!({ "agent": "nope", "provider_session_id": "provider-ses-0" }),
        )
        .await
        .unwrap();
    let error = unsupported.error.expect("unsupported delete is an error");
    assert!(error.message.contains("does not support"), "{error:?}");

    // Import the session so a local row exists, then delete the provider
    // transcript while the local metadata remains.
    let resumed = result(
        client
            .slow_request(
                method::AGENTS_PROVIDER_RESUME,
                json!({
                    "agent": "echo",
                    "provider_session_id": "provider-ses-0",
                    "cwd": daemon.work(),
                }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROVIDER_RESUME,
    );
    let local = resumed["session"]["id"].as_str().unwrap().to_string();
    result(
        client
            .slow_request(method::AGENTS_CLOSE, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_CLOSE,
    );

    let deleted = result(
        client
            .slow_request(
                method::AGENTS_PROVIDER_DELETE,
                json!({ "agent": "echo", "provider_session_id": "provider-ses-0" }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROVIDER_DELETE,
    );
    assert_eq!(deleted["deleted"], true);
    let marker = std::fs::read_to_string(daemon.root.join("deleted")).unwrap();
    assert_eq!(marker, "provider-ses-0");

    let history = result(
        client
            .slow_request(method::AGENTS_HISTORY, json!({ "limit": 10 }))
            .await
            .unwrap(),
        method::AGENTS_HISTORY,
    );
    assert!(
        history
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == local),
        "provider delete must not delete Favetto-local metadata"
    );

    // A provider-reported delete failure is surfaced.
    let failed = client
        .slow_request(
            method::AGENTS_PROVIDER_DELETE,
            json!({ "agent": "bad-delete", "provider_session_id": "provider-ses-0" }),
        )
        .await
        .unwrap();
    let error = failed.error.expect("delete failure is an error");
    assert!(error.message.contains("cannot delete session"), "{error:?}");
}

#[tokio::test]
async fn provider_session_ids_are_scoped_per_agent() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket).await;
    let client = connect(&daemon).await;
    let pool = open_db(&daemon.db_path()).await;

    let echo = result(
        client
            .slow_request(
                method::AGENTS_PROVIDER_RESUME,
                json!({
                    "agent": "echo",
                    "provider_session_id": "provider-ses-0",
                    "cwd": daemon.work(),
                }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROVIDER_RESUME,
    );
    let resumed = result(
        client
            .slow_request(
                method::AGENTS_PROVIDER_RESUME,
                json!({
                    "agent": "resume-only",
                    "provider_session_id": "provider-ses-0",
                    "cwd": daemon.work(),
                }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROVIDER_RESUME,
    );

    let echo_local = echo["session"]["id"].as_str().unwrap();
    let resumed_local = resumed["session"]["id"].as_str().unwrap();
    assert_ne!(
        echo_local, resumed_local,
        "the same provider id under two agents imports two local records"
    );
    assert_eq!(session_row(&pool, echo_local).await.unwrap().1, "echo");
    assert_eq!(
        session_row(&pool, resumed_local).await.unwrap().1,
        "resume-only"
    );

    // A live transcript cannot be deleted underneath its turn.
    let active = client
        .slow_request(
            method::AGENTS_PROVIDER_DELETE,
            json!({ "agent": "echo", "provider_session_id": "provider-ses-0" }),
        )
        .await
        .unwrap();
    assert!(
        active
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("is active")),
        "{:?}",
        active.error
    );

    // Deleting one agent's provider transcript leaves the other's row alone.
    for local in [echo_local, resumed_local] {
        result(
            client
                .slow_request(method::AGENTS_CLOSE, json!({ "session_id": local }))
                .await
                .unwrap(),
            method::AGENTS_CLOSE,
        );
    }
    result(
        client
            .slow_request(
                method::AGENTS_PROVIDER_DELETE,
                json!({ "agent": "echo", "provider_session_id": "provider-ses-0" }),
            )
            .await
            .unwrap(),
        method::AGENTS_PROVIDER_DELETE,
    );
    assert!(session_row(&pool, resumed_local).await.is_some());
}
