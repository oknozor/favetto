//! End-to-end test for the reference external supervisor (issue #153).
//!
//! Spawns the real `favetto` binary against an isolated temp dir and a fake echo
//! agent, then drives the two-step supervisor from `favetto-tui` over the Unix
//! socket. The daemon is untouched: the test only observes `workflow.inspect`,
//! decides `wait`/`spawn`, and submits `workflow.spawn` through the public RPCs.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use favetto_core::model::TaskStatus;
use favetto_core::rpc::method;
use favetto_core::workflow::WorkflowState;
use favetto_tui::client::{Client, Transport};
use favetto_tui::supervisor::{self, Action, Policy};

/// How long the daemon may take to start accepting connections.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// An isolated daemon plus its scratch workspace; dropping the harness kills the
/// child and removes everything it wrote.
struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    child: Child,
}

impl Daemon {
    /// Create a scratch workspace with a slow fake echo agent and the two
    /// supervisor demo tasks, then spawn `favetto daemon` against it.
    ///
    /// The fake agent sleeps briefly so the supervisor reliably observes the
    /// spawned step in `running` before it finishes — that is what makes the
    /// `wait` decision deterministic in the test.
    fn spawn() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "favetto-supervisor-e2e-{}-{nanos}",
            std::process::id()
        ));
        let data_dir = root.join("data");
        let tasks_dir = root.join("tasks");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let agent = root.join("echo-agent.sh");
        std::fs::write(
            &agent,
            "#!/bin/sh\nsleep 0.4\nprintf 'echo-agent: %s\\n' \"$1\"\nprintf 'ECHO_AGENT_DONE\\n'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        for name in ["supervisor_plan", "supervisor_implement"] {
            std::fs::write(
                tasks_dir.join(format!("{name}.md")),
                format!("agent = \"echo\"\n---\n{name}\n"),
            )
            .unwrap();
        }

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
            child,
        }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    /// The captured daemon log, for assertion failure messages.
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

/// A free loopback port. The listener is dropped immediately, so a small race
/// remains; acceptable for a test.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Block until the Unix socket accepts connections, or panic with the log.
async fn wait_for_socket(path: &Path, daemon: &Daemon) {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::UnixStream::connect(path).await {
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

#[tokio::test]
async fn reference_supervisor_drives_a_two_step_workflow() {
    let daemon = Daemon::spawn();
    wait_for_socket(daemon.socket(), &daemon).await;

    let client = Client::connect(Transport::Unix(daemon.socket().to_path_buf()))
        .await
        .expect("connect to the daemon");

    let policy = Policy {
        first_task: "supervisor_plan".to_string(),
        second_task: "supervisor_implement".to_string(),
        input: serde_json::Value::Null,
        dedupe_key: "supervisor-e2e:implement".to_string(),
        wait: Duration::from_millis(100),
        max_cycles: 300,
    };

    // The supervisor starts the first step as its own root, then drives it.
    let root_id = supervisor::start_root(&client, &policy)
        .await
        .expect("start the root task");
    let outcome = supervisor::run(&client, &policy, root_id)
        .await
        .expect("run the supervisor loop");

    assert_eq!(
        outcome.terminal.action,
        Action::Complete,
        "unexpected terminal decision: {outcome:?}"
    );
    assert!(
        outcome.decisions.iter().any(|d| d.action == Action::Wait),
        "the loop should have waited at least once: {outcome:?}"
    );
    assert!(
        outcome.decisions.iter().any(|d| d.action == Action::Spawn),
        "the loop should have spawned the second step: {outcome:?}"
    );

    // Both steps ran, in the same root, and the root is done.
    let view = supervisor::inspect(&client, root_id)
        .await
        .expect("inspect the root");
    assert_eq!(view.state, WorkflowState::Succeeded, "{view:?}");
    let plan = view
        .tasks
        .iter()
        .find(|t| t.name == "supervisor_plan")
        .expect("the plan step ran");
    let implement = view
        .tasks
        .iter()
        .find(|t| t.name == "supervisor_implement")
        .expect("the implement step was spawned");
    assert_eq!(plan.status, TaskStatus::Succeeded);
    assert_eq!(implement.status, TaskStatus::Succeeded);

    // The spawned step is a child of the same root: the supervisor created it,
    // not a catalog `spawn` edge.
    let row = client
        .request(method::TASKS_GET, serde_json::json!({ "id": implement.id }))
        .await
        .expect("tasks.get")
        .result
        .expect("tasks.get result");
    assert_eq!(row["root_id"], root_id.to_string());
    assert_eq!(row["parent_id"], root_id.to_string());
}
