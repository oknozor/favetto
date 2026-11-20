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
    child: Child,
}

impl Daemon {
    fn spawn() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "favetto-session-load-{}-{nanos}",
            std::process::id()
        ));
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

        let socket = root.join("favetto.sock");
        let log = std::fs::File::create(root.join("daemon.log")).unwrap();
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
            .arg("127.0.0.1:0")
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
                .request(
                    method::AGENTS_START,
                    json!({ "agent": "replay", "session_id": local }),
                )
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
    let loaded = result(load.await.expect("load task"), method::AGENTS_START);
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
                .request(
                    method::AGENTS_START,
                    json!({ "agent": "replay-bad", "session_id": local }),
                )
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

    let loaded = result(
        client
            .request(
                method::AGENTS_START,
                json!({
                    "agent": "echo",
                    "session_id": local,
                    "cwd": "/nonexistent/favetto-load-cwd",
                }),
            )
            .await
            .unwrap(),
        method::AGENTS_START,
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
