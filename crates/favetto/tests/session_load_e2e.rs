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

use favetto_core::rpc::method;
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
                 FAKE_ACP_LOAD_MARKER = {bad_load:?}\n"
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
