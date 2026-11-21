//! End-to-end coverage for ACP v1 plan and advertised-command snapshots.
//!
//! The fake agent emits a complete plan and command list on every prompt. The
//! daemon must expose the same current snapshot through `tasks.events` and
//! `agents.attach`, including when the caller asks only for events after the
//! backfill cursor, and must forward slash commands as ordinary prompt text.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::rpc::method;
use favetto_tui::client::{Client, Transport};
use serde_json::json;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(25);

struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    child: Child,
}

impl Daemon {
    fn spawn(prompt_marker: &Path) -> Self {
        let root = scratch_root("plan-commands");
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        let work_dir = root.join("work");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(&work_dir).unwrap();
        std::fs::write(
            tasks_dir.join("echo_task.md"),
            "agent = \"echo\"\n---\necho: hello-plan\n",
        )
        .unwrap();

        let agent = env!("CARGO_BIN_EXE_fake-acp-agent");
        let plan = r#"[{"content":"Read the issue","priority":"high","status":"completed"},{"content":"Implement the fix","priority":"medium","status":"in_progress"}]"#;
        let commands = r#"[{"name":"web","description":"Search the web","input":{"hint":"query"}},{"name":"test","description":"Run tests"}]"#;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\ndefault = \"echo\"\n\n\
                 [agents.echo]\ncommand = {agent:?}\nargs = []\n\
                 [agents.echo.env]\n\
                 FAKE_ACP_IGNORE_CLOSE = \"1\"\n\
                 FAKE_ACP_PLAN = {plan:?}\n\
                 FAKE_ACP_COMMANDS = {commands:?}\n\
                 FAKE_ACP_PROMPT_MARKER = {prompt_marker:?}\n"
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

fn result(resp: favetto_core::rpc::Response, method: &str) -> serde_json::Value {
    assert!(resp.error.is_none(), "{method} failed: {:?}", resp.error);
    resp.result.unwrap_or(serde_json::Value::Null)
}

async fn task_events(client: &Client, task_id: &str, after: Option<u64>) -> serde_json::Value {
    result(
        client
            .request(
                method::TASKS_EVENTS,
                json!({ "id": task_id, "after": after, "limit": 500 }),
            )
            .await
            .unwrap(),
        method::TASKS_EVENTS,
    )
}

#[tokio::test]
async fn attach_and_backfill_share_the_current_plan_and_commands() {
    let prompt_marker = std::env::temp_dir().join(format!(
        "favetto-plan-prompt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let daemon = Daemon::spawn(&prompt_marker);
    wait_for_socket(&daemon.socket).await;
    let client = Client::connect(Transport::Unix(daemon.socket.clone()))
        .await
        .expect("connect to the daemon");

    // A real task run makes `tasks.events` resolve a task-owned session.
    let started = result(
        client
            .request(method::TASKS_START, json!({ "name": "echo_task" }))
            .await
            .unwrap(),
        method::TASKS_START,
    );
    let task_id = started["id"].as_str().expect("task id").to_string();

    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    let (session_id, events_result) = loop {
        let result = task_events(&client, &task_id, None).await;
        let has_plan = result["plan"]
            .as_array()
            .is_some_and(|plan| plan.len() == 2);
        let has_commands = result["commands"]
            .as_array()
            .is_some_and(|commands| commands.len() == 2);
        if let (Some(session_id), true, true) =
            (result["session_id"].as_str(), has_plan, has_commands)
        {
            break (session_id.to_string(), result);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "plan/commands never appeared through tasks.events: {result}"
        );
        tokio::time::sleep(POLL).await;
    };

    assert_eq!(events_result["plan"][0]["content"], "Read the issue");
    assert_eq!(events_result["plan"][0]["status"], "completed");
    assert_eq!(events_result["plan"][1]["priority"], "medium");
    assert_eq!(events_result["commands"][0]["name"], "web");
    assert_eq!(events_result["commands"][0]["input_hint"], "query");
    assert_eq!(events_result["commands"][1]["name"], "test");
    assert!(
        events_result["events"]
            .as_array()
            .expect("events array")
            .iter()
            .any(|push| push["event"]["event"] == "plan"),
        "the plan event is still part of the ordered feed: {events_result}"
    );

    // `agents.attach` returns the same snapshot as the backfill.
    let attached = result(
        client
            .request(method::AGENTS_ATTACH, json!({ "session_id": session_id }))
            .await
            .unwrap(),
        method::AGENTS_ATTACH,
    );
    assert_eq!(attached["plan"], events_result["plan"]);
    assert_eq!(attached["commands"], events_result["commands"]);

    // Asking only for later events still returns the authoritative snapshot.
    let cursor = events_result["next_cursor"].as_u64().unwrap_or(0);
    let later = task_events(&client, &task_id, Some(cursor)).await;
    assert_eq!(later["plan"], events_result["plan"]);
    assert_eq!(later["commands"], events_result["commands"]);
    assert!(
        later["events"]
            .as_array()
            .is_some_and(|events| events.is_empty()),
        "the cursor is already at the tail: {later}"
    );

    // An interactive session forwards an unknown slash command verbatim.
    let _ = std::fs::remove_file(&prompt_marker);
    let interactive = result(
        client
            .request(
                method::AGENTS_START,
                json!({
                    "agent": "echo",
                    "cwd": daemon.work(),
                    "prompt": "/unknown arg with spaces"
                }),
            )
            .await
            .unwrap(),
        method::AGENTS_START,
    );
    let local = interactive["session"]["id"]
        .as_str()
        .expect("interactive session id")
        .to_string();
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if prompt_marker.exists() {
            let seen = std::fs::read_to_string(&prompt_marker).unwrap();
            assert_eq!(seen, "/unknown arg with spaces");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the fake agent never received the prompt"
        );
        tokio::time::sleep(POLL).await;
    }

    // The same live session exposes plan/commands through attach.
    let attached = result(
        client
            .request(method::AGENTS_ATTACH, json!({ "session_id": local }))
            .await
            .unwrap(),
        method::AGENTS_ATTACH,
    );
    assert_eq!(attached["plan"][0]["content"], "Read the issue");
    assert_eq!(attached["commands"][0]["name"], "web");

    let _ = std::fs::remove_file(prompt_marker);
}
