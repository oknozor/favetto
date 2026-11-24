//! Interop leg: run a Favetto task against a real, SDK-implemented ACP agent
//! instead of the in-repo fake — the client-side counterpart of the acp-tck
//! `cross-check` legs (which score agents). The acp-tck itself only drives
//! agents, so Favetto's client role is exercised here, in the `nextest` CI job,
//! against an agent that is independently TCK-conformant.
//!
//! The agent command comes from `FAVETTO_INTEROP_AGENT` (a single command line,
//! split on whitespace). The test is skipped when the variable is unset; the
//! `acp-tck` workflow's `interop` job sets it to the pinned ACP Python SDK's
//! `examples/echo_agent.py` (run via `uv run`, which honors its inline PEP 723
//! metadata).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::rpc::method;
use favetto_tui::client::{Client, Transport};
use serde_json::{json, Value};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Real agents are slower than the fake: allow a full minute for one turn.
const TURN_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);

fn interop_agent_command() -> Option<Vec<String>> {
    let raw = std::env::var("FAVETTO_INTEROP_AGENT").ok()?;
    let parts: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    child: Child,
}

impl Daemon {
    fn spawn(agent_command: &[String]) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "favetto-acp-interop-{}-{nanos}",
            std::process::id()
        ));
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let command = &agent_command[0];
        let args: Vec<String> = agent_command[1..].to_vec();
        let args_toml = serde_json::to_string(&args).unwrap();
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"interop\"\n\n\
                 [agents.interop]\ncommand = {command:?}\nargs = {args_toml}\n"
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
                tokio::time::sleep(POLL).await;
            }
            Err(error) => panic!(
                "daemon socket {} never became ready: {error}\n--- daemon log ---\n{}",
                path.display(),
                daemon.log()
            ),
        }
    }
}

async fn call(client: &Client, method: &str, params: Value) -> Value {
    let response = client
        .request(method, params)
        .await
        .unwrap_or_else(|error| panic!("{method} failed: {error}"));
    assert!(
        response.error.is_none(),
        "{method} returned an error: {:?}",
        response.error
    );
    response.result.unwrap_or(Value::Null)
}

#[tokio::test]
async fn favetto_client_completes_a_task_against_a_real_sdk_agent() {
    let Some(agent_command) = interop_agent_command() else {
        eprintln!("FAVETTO_INTEROP_AGENT not set; skipping interop test");
        return;
    };
    let daemon = Daemon::spawn(&agent_command);
    wait_for_socket(&daemon.socket, &daemon).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");

    std::fs::write(
        daemon.root.join("tasks").join("interop.md"),
        "agent = \"interop\"\n---\ninterop-probe-echo-me\n",
    )
    .unwrap();
    let started = call(&client, method::TASKS_START, json!({ "name": "interop" })).await;
    let task_id = started["id"].as_str().expect("task id").to_string();

    // Wait for the turn to finish. The echo agent is fast, so don't rely on
    // the live session showing in agents.list — the persisted task record
    // carries the ACP session id the real agent returned from `session/new`,
    // which proves the full handshake (initialize → session/new) ran against
    // a real SDK agent.
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    let task = loop {
        let task = call(&client, method::TASKS_GET, json!({ "id": task_id })).await;
        if task["status"] == json!("succeeded") || task["status"] == json!("failed") {
            break task;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "task never finished: {task:?}\n--- daemon log ---\n{}",
            daemon.log()
        );
        tokio::time::sleep(POLL).await;
    };
    assert_eq!(
        task["status"],
        json!("succeeded"),
        "task against the real SDK agent failed: {task:?}\n--- daemon log ---\n{}",
        daemon.log()
    );
    assert!(
        task["session_id"].as_str().is_some_and(|id| !id.is_empty()),
        "no ACP session id recorded on the task: {task:?}"
    );
}
