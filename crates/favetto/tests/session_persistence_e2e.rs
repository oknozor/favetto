//! Durable ACP session metadata across every launch path.
//!
//! Spawns the real daemon against a fake ACP agent and checks the
//! `agent_sessions` rows for an interactive `agents.start`, a one-shot task, and
//! a headless catalog run. The headless agent is gated mid-turn so the test can
//! observe that the ACP id is already durable *before* the task is finalized.

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

/// An isolated daemon plus the fake agent it launches.
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
            "favetto-session-persistence-{}-{nanos}",
            std::process::id()
        ));
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        std::fs::write(
            tasks_dir.join("echo_task.md"),
            "agent = \"echo\"\n---\necho: persistence\n",
        )
        .unwrap();

        // The gate makes the prompt turn block, so the test can inspect the
        // durable row while the task is still running.
        let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"echo\"\n\n\
                 [agents.echo]\ncommand = {:?}\nargs = []\n\
                 [agents.echo.env]\nFAKE_ACP_GATE_DIR = {:?}\nFAKE_ACP_TITLE = \"Echo session\"\n",
                agent, root
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

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn wait_for_socket(path: &Path, daemon: &Daemon) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => return,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!(
                "daemon socket {} never became ready: {error}\n--- daemon log ---\n{}",
                path.display(),
                daemon.log()
            ),
        }
    }
}

async fn wait_for_file(path: &Path) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    while !path.exists() {
        if tokio::time::Instant::now() >= deadline {
            panic!("{} never appeared", path.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn open_db(path: &Path) -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .expect("open the daemon database")
}

/// A row read from `agent_sessions`.
struct SessionRow {
    id: String,
    external_session_id: Option<String>,
    task_id: Option<String>,
    run_id: Option<String>,
    title: Option<String>,
    status: String,
}

async fn session_row(pool: &SqlitePool, id: &str) -> Option<SessionRow> {
    sqlx::query(
        "SELECT id, external_session_id, task_id, run_id, title, status
         FROM agent_sessions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap()
    .map(|row| SessionRow {
        id: row.get("id"),
        external_session_id: row.get("external_session_id"),
        task_id: row.get("task_id"),
        run_id: row.get("run_id"),
        title: row.get("title"),
        status: row.get("status"),
    })
}

async fn wait_for_session<F>(pool: &SqlitePool, id: &str, mut ready: F) -> SessionRow
where
    F: FnMut(&SessionRow) -> bool,
{
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(row) = session_row(pool, id).await {
            if ready(&row) {
                return row;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let row = session_row(pool, id).await;
            panic!(
                "session {id} did not reach the expected state; last row: {:?}",
                row.map(|row| (row.status, row.external_session_id))
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn wait_for_status(pool: &SqlitePool, id: &str, status: &str) -> SessionRow {
    wait_for_session(pool, id, |row| row.status == status).await
}

async fn wait_for_task(pool: &SqlitePool, id: &str, status: &str) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        let row = sqlx::query("SELECT status FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap();
        if row.is_some_and(|row| row.get::<String, _>("status") == status) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("task {id} did not reach {status}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn run_sql_id(pool: &SqlitePool, id: &str) -> Option<String> {
    let row = sqlx::query("SELECT session_id FROM task_runs WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()?;
    row.get("session_id")
}

fn result(resp: favetto_core::rpc::Response, method: &str) -> serde_json::Value {
    assert!(resp.error.is_none(), "{method} failed: {:?}", resp.error);
    resp.result.unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn every_launch_path_records_durable_session_metadata() {
    let daemon = Daemon::spawn();
    wait_for_socket(&daemon.socket, &daemon).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");
    let pool = open_db(&daemon.db_path()).await;
    let cwd = daemon.root.to_string_lossy().into_owned();

    // 1. Interactive `agents.start`: a row exists before the session is ready,
    //    then carries the agent's ACP id, and survives `agents.close`.
    let started = result(
        client
            .request(method::AGENTS_START, json!({ "agent": "echo", "cwd": cwd }))
            .await
            .unwrap(),
        method::AGENTS_START,
    );
    let interactive = started["session"]["id"].as_str().unwrap().to_string();
    let row = wait_for_session(&pool, &interactive, |row| {
        row.status == "ready" && row.external_session_id.is_some()
    })
    .await;
    assert_eq!(row.id, interactive);
    let interactive_acp = row.external_session_id.clone().unwrap();
    assert_ne!(
        interactive, interactive_acp,
        "the Favetto-local id must not be the agent's ACP id"
    );
    assert!(
        interactive_acp.starts_with("ses_fake_acp"),
        "{interactive_acp}"
    );
    assert!(row.task_id.is_none(), "an ad-hoc session has no task");
    assert!(row.run_id.is_none());

    result(
        client
            .request(method::AGENTS_CLOSE, json!({ "session_id": interactive }))
            .await
            .unwrap(),
        method::AGENTS_CLOSE,
    );
    let closed = wait_for_status(&pool, &interactive, "ended").await;
    assert_eq!(
        closed.external_session_id.as_deref(),
        Some(interactive_acp.as_str()),
        "closing keeps the identity for history"
    );

    // 2. One-shot interactive task: the row links to the inline task, has no
    //    run, and also survives the live entry.
    let oneshot = result(
        client
            .request(
                method::TASKS_START_ONESHOT,
                json!({ "agent": "echo", "cwd": cwd }),
            )
            .await
            .unwrap(),
        method::TASKS_START_ONESHOT,
    );
    let oneshot_session = oneshot["session"]["id"].as_str().unwrap().to_string();
    let oneshot_task = oneshot["task"]["id"].as_str().unwrap().to_string();
    let row = wait_for_session(&pool, &oneshot_session, |row| {
        row.status == "ready" && row.external_session_id.is_some()
    })
    .await;
    assert_eq!(row.task_id.as_deref(), Some(oneshot_task.as_str()));
    assert!(row.run_id.is_none());

    result(
        client
            .request(
                method::AGENTS_CLOSE,
                json!({ "session_id": oneshot_session }),
            )
            .await
            .unwrap(),
        method::AGENTS_CLOSE,
    );
    wait_for_status(&pool, &oneshot_session, "ended").await;

    // 3. Headless catalog task: the agent blocks inside its prompt turn, so the
    //    row and the run's session id are observable while the task still runs.
    let headless = result(
        client
            .request(method::TASKS_START, json!({ "name": "echo_task" }))
            .await
            .unwrap(),
        method::TASKS_START,
    );
    let headless_task = headless["id"].as_str().unwrap().to_string();
    wait_for_file(&daemon.root.join("agent-ready")).await;

    let session_id: String = {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        loop {
            let row = sqlx::query(
                "SELECT id FROM agent_sessions WHERE task_id = ? ORDER BY created_at DESC LIMIT 1",
            )
            .bind(&headless_task)
            .fetch_optional(&pool)
            .await
            .unwrap();
            if let Some(row) = row {
                break row.get("id");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no session row for task {headless_task}"
            );
            tokio::time::sleep(POLL).await;
        }
    };
    let row = wait_for_session(&pool, &session_id, |row| {
        row.status == "ready" && row.external_session_id.is_some()
    })
    .await;
    let headless_acp = row.external_session_id.clone().unwrap();
    assert_eq!(row.task_id.as_deref(), Some(headless_task.as_str()));
    let run_id = row
        .run_id
        .clone()
        .expect("a headless session links its task run");

    // The ACP id is durable on the run and the turn is still in flight.
    assert_eq!(
        run_sql_id(&pool, &run_id).await.as_deref(),
        Some(headless_acp.as_str()),
        "the run carries the ACP id before finalization"
    );
    let task = result(
        client
            .request(method::TASKS_GET, json!({ "id": headless_task }))
            .await
            .unwrap(),
        method::TASKS_GET,
    );
    assert_eq!(
        task["status"], "running",
        "the task must not be finalized yet"
    );

    // Title updates are persisted too.
    let row = wait_for_session(&pool, &session_id, |row| row.title.is_some()).await;
    assert_eq!(row.title.as_deref(), Some("Echo session"));

    // Release the turn and let the headless run finalize.
    std::fs::write(daemon.root.join("agent-release"), b"release").unwrap();
    wait_for_task(&pool, &headless_task, "succeeded").await;
    let ended = wait_for_status(&pool, &session_id, "ended").await;
    assert_eq!(
        ended.external_session_id.as_deref(),
        Some(headless_acp.as_str())
    );
    assert_eq!(
        run_sql_id(&pool, &run_id).await.as_deref(),
        Some(headless_acp.as_str()),
        "finalization keeps the id recorded at launch"
    );

    // The durable records outlive every live manager entry.
    assert!(session_row(&pool, &interactive).await.is_some());
    assert!(session_row(&pool, &oneshot_session).await.is_some());
    assert!(session_row(&pool, &session_id).await.is_some());
}
